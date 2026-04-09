use {
    js_sys::{Array, Uint8Array},
    lightningcss::{
        error::{Error as LightningCssError, PrinterErrorKind},
        rules::CssRule,
        stylesheet::{PrinterOptions, StyleSheet},
        values::url::Url,
        visit_types,
        visitor::{Visit, VisitTypes, Visitor},
    },
    std::{
        collections::HashMap,
        io::{self, Read, Seek},
        string::FromUtf8Error,
    },
    thiserror::Error as ThisError,
    web_sys::{Blob, BlobPropertyBag, Url as WebSysUrl, wasm_bindgen::JsValue},
    zip::{ZipArchive, result::ZipError},
};

// This is needed because the url in an @import isn't visited during a url
// visit, but if we try to visit Urls and Rules at the same time, then Urls
// within rules aren't visited.
#[derive(Clone, Copy)]
enum VisitPass {
    Urls,
    Rules,
}

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("can't read zip file: {0}")]
    CantReadZipFile(ZipError),

    #[error("can't find: {0}")]
    CantFind(ZipError),

    #[error("can't read: {0}")]
    CantRead(io::Error),

    #[error("can't parse: {0}")]
    CantParse(String),

    #[error("can't serialize: {0}")]
    CantSerialize(LightningCssError<PrinterErrorKind>),

    #[error("loop detected looking up {0}")]
    LoopDetected(String),

    #[error("can't convert to utf-8: #{0}")]
    NotUtf8(FromUtf8Error),

    #[error("can't create blob: #{0:?}")]
    BlobConstruction(JsValue),

    #[error("can't create object_url: #{0:?}")]
    ObjectUrlCreation(JsValue),

    #[cfg(not(feature = "grass"))]
    #[error("needs grass feature enabled: #{0}")]
    NeedsGrass(String),

    #[cfg(feature = "grass")]
    #[error("can't parse sass: #{0:?}")]
    CantParseSass(Box<grass::Error>),
}

#[derive(Clone)]
enum ObjUrlHolder {
    Computing,
    Computed(String),
}

impl From<ObjUrlHolder> for Option<String> {
    fn from(v: ObjUrlHolder) -> Option<String> {
        match v {
            ObjUrlHolder::Computing => None,
            ObjUrlHolder::Computed(s) => Some(s),
        }
    }
}

struct Builder<R> {
    archive: ZipArchive<R>,
    object_urls: HashMap<String, ObjUrlHolder>,
    visit_pass: Option<VisitPass>,
}

fn object_url_for(data: &[u8], mime_type: Option<&str>) -> Result<String, Error> {
    let uint8_array = Uint8Array::new_with_length(data.len() as u32);
    uint8_array.copy_from(data);
    let parts = Array::new();
    parts.push(&uint8_array);
    let properties = BlobPropertyBag::new();
    if let Some(t) = mime_type {
        properties.set_type(t);
    }
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &properties)
        .map_err(Error::BlobConstruction)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(Error::ObjectUrlCreation)?;
    Ok(url)
}

fn type_for(url: &str) -> Option<&'static str> {
    #[cfg(feature = "mime_guess")]
    {
        mime_guess::from_path(url).first_raw()
    }

    #[cfg(not(feature = "mime_guess"))]
    if url.ends_with(".svg") {
        Some("image/svg+xml")
    } else {
        None
    }
}

impl<R: Read + Seek> Builder<R> {
    fn new(archive: ZipArchive<R>) -> Self {
        Self {
            archive,
            object_urls: Default::default(),
            visit_pass: Default::default(),
        }
    }

    fn data(&mut self, filename: &str) -> Result<Vec<u8>, Error> {
        use Error::*;

        let mut file = self.archive.by_name(filename).map_err(CantFind)?;
        let is_sass = filename.ends_with(".scss") || filename.ends_with(".sass");
        #[cfg(not(feature = "grass"))]
        if is_sass {
            return Err(NeedsGrass(filename.to_string()));
        }
        let needs_css_processing = is_sass || filename.ends_with(".css");

        if needs_css_processing {
            let css_source = {
                let mut css_source = String::new();
                file.read_to_string(&mut css_source).map_err(CantRead)?;
                drop(file); // Shouldn't be needed, IMO
                #[cfg(feature = "grass")]
                if is_sass {
                    css_source = grass::from_string(css_source, &grass::Options::default())
                        .map_err(Error::CantParseSass)?;
                }
                css_source
            };
            let mut parsed_css = StyleSheet::parse(&css_source, Default::default())
                .map_err(|e| CantParse(e.to_string()))?;

            let incoming_visit_pass = self.visit_pass;

            self.visit_pass = Some(VisitPass::Urls);
            parsed_css.visit(self)?;

            self.visit_pass = Some(VisitPass::Rules);
            parsed_css.visit(self)?;

            self.visit_pass = incoming_visit_pass;

            Ok(parsed_css
                .to_css(PrinterOptions {
                    minify: true,
                    ..Default::default()
                })
                .map_err(CantSerialize)?
                .code
                .into())
        } else {
            let mut result = vec![0; file.size().try_into().unwrap()];
            file.read_exact(&mut result).map_err(CantRead)?;
            Ok(result)
        }
    }

    fn into_object_urls(self) -> Vec<String> {
        self.object_urls
            .into_values()
            .filter_map(Into::into)
            .collect()
    }

    fn object_url(&mut self, url: &str) -> Result<String, Error> {
        use ObjUrlHolder::*;

        if let Some(v) = self.object_urls.get(url) {
            return match v {
                Computing => Err(Error::LoopDetected(url.to_string())),
                Computed(v) => Ok(v.to_string()),
            };
        }
        self.object_urls.insert(url.to_string(), Computing);
        let object_url = object_url_for(&self.data(url)?, type_for(url))?;
        *(self.object_urls.get_mut(url).unwrap()) = Computed(object_url.clone());
        Ok(object_url)
    }
}

impl<'i, R: Read + Seek> Visitor<'i> for Builder<R> {
    type Error = Error;

    fn visit_types(&self) -> VisitTypes {
        match &self.visit_pass {
            None => unreachable!(),
            Some(VisitPass::Urls) => visit_types!(URLS),
            Some(VisitPass::Rules) => visit_types!(RULES),
        }
    }

    fn visit_url(&mut self, url: &mut Url<'i>) -> Result<(), Self::Error> {
        match self.object_url(&url.url) {
            Ok(u) => url.url = u.into(),
            Err(Error::CantFind(_)) => {}
            Err(e) => return Err(e),
        }
        Ok(())
    }

    // Need visit_rule because the url in an @import isn't visited by visit_url
    fn visit_rule(&mut self, rule: &mut CssRule<'i>) -> Result<(), Self::Error> {
        if let CssRule::Import(import) = rule {
            if import.url.starts_with("blob:") {
                // info!("looks like @import urls are now visited");
            } else {
                import.url = self.object_url(&import.url)?.into();
            }
        }
        Ok(())
    }
}

/*
// TODO: get rid of named top levels and require new to pass in a name.
// If I want to bless certain names for Mb2, I should do it outside this crate.
const PRIMARY_TOP_LEVEL: &str = "style.css";
const ALTERNATE_TOP_LEVEL: &str = "main.css";

fn top_level<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    stem: &Option<&str>,
) -> Result<String, Error> {
    let (mut file, name): (ZipFile<R>, Cow<str>) = if let Some(stem) = stem
        && let desired = format!("{stem}.css")
        && let Ok(f) = archive.by_name(&desired)
    {
        (f, desired.into())
    } else if let Ok(f) = archive.by_name(PRIMARY_TOP_LEVEL) {
        (f, PRIMARY_TOP_LEVEL.into())
    } else if let Ok(f) = archive.by_name(ALTERNATE_TOP_LEVEL) {
        (f, ALTERNATE_TOP_LEVEL.into())
    } else {
        return Err(Error::NoTopLevel);
    };

    let mut css = String::new();
    file.read_to_string(&mut css)
        .map_err(|e| Error::CantReadTopLevel(name.into_owned(), e))?;
    Ok(css)
}
*/

pub struct AssetPack {
    data: String,
    object_urls: Vec<String>,
}

impl AssetPack {
    pub fn new<R: Read + Seek>(reader: R, top_level: &str) -> Result<Self, Error> {
        use Error::*;

        let mut builder = Builder::new(ZipArchive::new(reader).map_err(CantReadZipFile)?);
        Ok(Self {
            data: builder.data(top_level)?.try_into().map_err(NotUtf8)?,
            object_urls: builder.into_object_urls(),
        })
    }

    pub fn data(&self) -> &str {
        &self.data
    }
}

impl Drop for AssetPack {
    fn drop(&mut self) {
        for url in &self.object_urls {
            let _ = WebSysUrl::revoke_object_url(url);
        }
    }
}

#[cfg(test)]
mod tests {
    use {regex::Regex, std::sync::LazyLock};
    const NODE_BLOB_PATTERN: &str =
        "blob:nodedata:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";

    // In theory wasm-pack test --node
    // or wasm-pack test --headless --firefox
    // work, although wasm-pack test --headless --chrome doesn't,
    // although wasm-pack --chrome does, although it requires manually
    // visiting http://127.0.0.1:8000

    use {super::*, std::io::Cursor, wasm_bindgen_test::*};
    // I don't yet know if we want the following line, seeing how we
    // don't yet have tests.
    // wasm_bindgen_test::wasm_bindgen_test_configure! {run_in_browser}

    #[wasm_bindgen_test]
    fn it_works() {
        // This is fragile and foolhardy, because if the match fails,
        // we won't know why.  Additionally, what I've currently baked
        // into the test data is a single @import, but it makes sense
        // to have an @import, a @font-family and something that uses
        // a background and perhaps for us to look at
        // object_urls.len().  OTOH, I wanted at least one successful
        // test just so I wouldn't totally forget about tests, so it
        // serves that purpose.

        static REGEX: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(&format!("@import \"{}\";", NODE_BLOB_PATTERN)).unwrap());

        let zip = Cursor::new(include_bytes!("../test-data/alternate.zip"));
        let pack = AssetPack::new(zip, "alternate.css").unwrap();
        assert!(REGEX.is_match(pack.data()));
    }
}
