#[cfg(not(target_arch = "wasm32"))]
use base64::{Engine as _, engine::general_purpose::STANDARD};
#[cfg(target_arch = "wasm32")]
use {
    js_sys::{Array, Uint8Array},
    web_sys::{Blob, BlobPropertyBag, Url as WebSysUrl, wasm_bindgen::JsValue},
};
use {
    lightningcss::{
        declaration::DeclarationBlock,
        error::{Error as LightningCssError, ParserError, PrinterErrorKind},
        properties::{
            Property,
            custom::{CustomPropertyName, Token, TokenList, TokenOrValue, UnresolvedColor},
        },
        rules::{CssRule, Location},
        stylesheet::{ParserOptions, PrinterOptions, StyleSheet},
        values::url::Url,
        visit_types,
        visitor::{Visit, VisitTypes, Visitor},
    },
    std::{
        collections::HashMap,
        io::{self, Read, Seek},
        string::FromUtf8Error,
        sync::{Arc, RwLock},
    },
    thiserror::Error as ThisError,
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

    #[cfg(target_arch = "wasm32")]
    #[error("can't create blob: #{0:?}")]
    BlobConstruction(JsValue),

    #[cfg(target_arch = "wasm32")]
    #[error("can't create object_url: #{0:?}")]
    ObjectUrlCreation(JsValue),

    #[cfg(not(feature = "sasso"))]
    #[error("needs sasso feature enabled: #{0}")]
    NeedsSasso(String),

    #[cfg(feature = "sasso")]
    #[error("can't parse sass: #{0:?}")]
    CantParseSass(sasso::Error),
}

/// A CSS syntax error, located within the CSS that was checked.
///
/// `line` and `column` are both 1-based, and are 0 when the underlying error
/// carries no location. lightningcss numbers lines from 0 and columns from 1,
/// so the line it reports is incremented here.
#[derive(ThisError, Debug, Clone, PartialEq, Eq)]
#[error("{filename}:{line}:{column}: {message}")]
pub struct CssSyntaxError {
    pub filename: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

/// Collects the errors the parser recovers from rather than failing on.
type Warnings<'i> = Arc<RwLock<Vec<LightningCssError<ParserError<'i>>>>>;

fn parse_stylesheet<'i>(
    css: &'i str,
    filename: &str,
    warnings: Option<Warnings<'i>>,
) -> Result<StyleSheet<'i>, LightningCssError<ParserError<'i>>> {
    StyleSheet::parse(
        css,
        ParserOptions {
            filename: filename.to_string(),
            warnings,
            ..Default::default()
        },
    )
}

fn syntax_error(error: LightningCssError<ParserError>, filename: &str) -> CssSyntaxError {
    let message = error.kind.to_string();

    match error.loc {
        Some(loc) => CssSyntaxError {
            filename: loc.filename,
            line: loc.line + 1,
            column: loc.column,
            message,
        },
        None => CssSyntaxError {
            filename: filename.to_string(),
            line: 0,
            column: 0,
            message,
        },
    }
}

/// Real CSS properties this lightningcss has no parser for.
///
/// It keeps a declaration whose name it does not know as a custom property, so
/// [`check_css`] reports every unlisted one as a typo. A property browsers
/// support but lightningcss does not know belongs here — the one-line fix when
/// a newly adopted property is reported. A vendor prefix earns no exemption of
/// its own, since a typo can be prefixed too, so `-ms-overflow-style` is
/// listed like any other name. Keep this sorted.
pub const KNOWN_UNLISTED_PROPERTIES: &[&str] = &[
    "-ms-overflow-style",
    "border-collapse",
    "content",
    "float",
    "font-variant-numeric",
    "overflow-anchor",
    "pointer-events",
    "position-anchor",
    "position-area",
    "scrollbar-width",
    "will-change",
];

/// The `@counter-style` descriptors, which this lightningcss keeps in a plain
/// declaration block because it has no parser for any of them. An unknown name
/// in that block is measured against these instead of the property list. Keep
/// this sorted.
const COUNTER_STYLE_DESCRIPTORS: [&str; 10] = [
    "additive-symbols",
    "fallback",
    "negative",
    "pad",
    "prefix",
    "range",
    "speak-as",
    "suffix",
    "symbols",
    "system",
];

/// The `@page` descriptors, which its block holds alongside real properties.
/// Keep this sorted.
const PAGE_DESCRIPTORS: [&str; 4] = ["bleed", "marks", "page-orientation", "size"];

/// The CSS-wide keywords. Every typed property parser but `all`'s rejects
/// them, leaving a perfectly valid declaration unparsed.
const CSS_WIDE_KEYWORDS: [&str; 5] = ["inherit", "initial", "revert", "revert-layer", "unset"];

/// Whether `tokens` reference a `var()` or an `env()` anywhere within, which
/// lightningcss cannot parse however well formed the value is. A reference
/// nested in a function's arguments or in an `rgb()`/`hsl()` alpha or a
/// `light-dark()` component counts; a `var()`/`env()` fallback needs no walk of
/// its own, being inside a token that already matches.
fn contains_var_or_env(tokens: &TokenList) -> bool {
    tokens.0.iter().any(|token| match token {
        TokenOrValue::Var(_) | TokenOrValue::Env(_) => true,
        TokenOrValue::Function(function) => contains_var_or_env(&function.arguments),
        TokenOrValue::UnresolvedColor(color) => match color {
            UnresolvedColor::RGB { alpha, .. } | UnresolvedColor::HSL { alpha, .. } => {
                contains_var_or_env(alpha)
            }
            UnresolvedColor::LightDark { light, dark } => {
                contains_var_or_env(light) || contains_var_or_env(dark)
            }
        },
        _ => false,
    })
}

/// The one identifier `tokens` amount to, ignoring surrounding whitespace.
fn lone_ident<'a>(tokens: &'a TokenList) -> Option<&'a str> {
    let mut idents = tokens.0.iter().filter(|token| !token.is_whitespace());

    match (idents.next(), idents.next()) {
        (Some(TokenOrValue::Token(Token::Ident(ident))), None) => Some(ident.as_ref()),
        _ => None,
    }
}

/// Whether `tokens` are a lone CSS-wide keyword, valid on every property.
fn is_css_wide_keyword(tokens: &TokenList) -> bool {
    lone_ident(tokens).is_some_and(|ident| {
        CSS_WIDE_KEYWORDS
            .iter()
            .any(|keyword| ident.eq_ignore_ascii_case(keyword))
    })
}

/// Whether `tokens` are the `none` that the shadow properties' list parsers
/// leave unparsed although it is valid on both.
fn is_shadow_none(name: &str, tokens: &TokenList) -> bool {
    matches!(name, "text-shadow" | "box-shadow")
        && lone_ident(tokens).is_some_and(|ident| ident.eq_ignore_ascii_case("none"))
}

/// Where `rule` starts, for the rule kinds that carry a location. The kinds
/// that do not are reported at the last location seen, which for a nested rule
/// is the rule enclosing it.
fn rule_location(rule: &CssRule) -> Option<Location> {
    match rule {
        CssRule::Media(rule) => Some(rule.loc),
        CssRule::Import(rule) => Some(rule.loc),
        CssRule::Style(rule) => Some(rule.loc),
        CssRule::Keyframes(rule) => Some(rule.loc),
        CssRule::FontFace(rule) => Some(rule.loc),
        CssRule::FontPaletteValues(rule) => Some(rule.loc),
        CssRule::FontFeatureValues(rule) => Some(rule.loc),
        CssRule::Page(rule) => Some(rule.loc),
        CssRule::Supports(rule) => Some(rule.loc),
        CssRule::CounterStyle(rule) => Some(rule.loc),
        CssRule::Namespace(rule) => Some(rule.loc),
        CssRule::MozDocument(rule) => Some(rule.loc),
        CssRule::Nesting(rule) => Some(rule.loc),
        CssRule::NestedDeclarations(rule) => Some(rule.loc),
        CssRule::Viewport(rule) => Some(rule.loc),
        CssRule::CustomMedia(rule) => Some(rule.loc),
        CssRule::LayerStatement(rule) => Some(rule.loc),
        CssRule::LayerBlock(rule) => Some(rule.loc),
        CssRule::Property(rule) => Some(rule.loc),
        CssRule::Container(rule) => Some(rule.loc),
        CssRule::Scope(rule) => Some(rule.loc),
        CssRule::StartingStyle(rule) => Some(rule.loc),
        CssRule::ViewTransition(rule) => Some(rule.loc),
        CssRule::PositionTry(rule) => Some(rule.loc),
        CssRule::Unknown(rule) => Some(rule.loc),
        _ => None,
    }
}

/// What the declarations of the rule being visited are. `@counter-style`,
/// `@page` and `@viewport` store descriptors in the same declaration block a
/// style rule stores properties in, so the check has to tell them apart.
#[derive(Clone, Copy)]
enum DeclarationContext {
    /// Ordinary properties, `@position-try`'s declarations included.
    Properties,
    CounterStyle,
    Page,
    /// A deprecated `@viewport`, whose block is not checked at all.
    Viewport,
}

impl DeclarationContext {
    fn of(rule: &CssRule) -> Self {
        match rule {
            CssRule::CounterStyle(_) => Self::CounterStyle,
            CssRule::Page(_) => Self::Page,
            CssRule::Viewport(_) => Self::Viewport,
            _ => Self::Properties,
        }
    }
}

/// Fails on the first declaration [`check_css`] considers a typo. A
/// declaration carries no location of its own, so rules are visited to track
/// the enclosing one's location and the kind of declarations it holds.
struct DeclarationCheck<'a> {
    filename: &'a str,
    loc: Option<Location>,
    context: DeclarationContext,
}

impl DeclarationCheck<'_> {
    fn error(&self, message: String) -> CssSyntaxError {
        let (line, column) = self.loc.map_or((0, 0), |loc| (loc.line + 1, loc.column));

        CssSyntaxError {
            filename: self.filename.to_string(),
            line,
            column,
            message,
        }
    }

    fn check(&self, property: &Property) -> Result<(), CssSyntaxError> {
        match property {
            Property::Unparsed(unparsed) => {
                let name = unparsed.property_id.name();
                if contains_var_or_env(&unparsed.value)
                    || is_css_wide_keyword(&unparsed.value)
                    || is_shadow_none(name, &unparsed.value)
                {
                    return Ok(());
                }
                let value = property
                    .value_to_css_string(PrinterOptions::default())
                    .unwrap_or_default();
                Err(self.error(format!("unparseable value for `{name}`: `{value}`")))
            }
            Property::Custom(custom) => {
                let CustomPropertyName::Unknown(ident) = &custom.name else {
                    return Ok(());
                };
                self.check_unknown_name(ident.as_ref())
            }
            _ => Ok(()),
        }
    }

    /// Whether `name`, which no property parser claimed, is valid where it
    /// appears: a descriptor of the enclosing rule, or a real property
    /// lightningcss does not know.
    fn check_unknown_name(&self, name: &str) -> Result<(), CssSyntaxError> {
        match self.context {
            DeclarationContext::CounterStyle if COUNTER_STYLE_DESCRIPTORS.contains(&name) => Ok(()),
            DeclarationContext::CounterStyle => {
                Err(self.error(format!("unknown @counter-style descriptor `{name}`")))
            }
            DeclarationContext::Page if PAGE_DESCRIPTORS.contains(&name) => Ok(()),
            _ if KNOWN_UNLISTED_PROPERTIES.contains(&name) => Ok(()),
            _ => Err(self.error(format!(
                "unknown property `{name}` (a real property lightningcss \
                 does not know? add it to KNOWN_UNLISTED_PROPERTIES)"
            ))),
        }
    }
}

impl<'i> Visitor<'i> for DeclarationCheck<'_> {
    type Error = CssSyntaxError;

    fn visit_types(&self) -> VisitTypes {
        visit_types!(RULES | PROPERTIES)
    }

    fn visit_rule(&mut self, rule: &mut CssRule<'i>) -> Result<(), Self::Error> {
        if let Some(loc) = rule_location(rule) {
            self.loc = Some(loc);
        }
        let enclosing = std::mem::replace(&mut self.context, DeclarationContext::of(rule));
        let result = rule.visit_children(self);
        self.context = enclosing;
        result
    }

    fn visit_declaration_block(
        &mut self,
        block: &mut DeclarationBlock<'i>,
    ) -> Result<(), Self::Error> {
        if matches!(self.context, DeclarationContext::Viewport) {
            return Ok(());
        }
        for property in block
            .declarations
            .iter()
            .chain(block.important_declarations.iter())
        {
            self.check(property)?;
        }
        Ok(())
    }
}

/// Reports the first syntax error in `css`, if any, using the parser asset
/// packs are built with. `filename` only names the CSS in the returned error.
///
/// Unlike a pack build, the check also reports what the parser keeps rather
/// than fails on: the errors it recovers from, e.g. an unknown at-rule, and
/// the declarations it stores verbatim instead of rejecting — a value the
/// property's own parser cannot parse (`width: 10pxx`, `color: redd`,
/// `color: ;`) and an unknown property name (`colr: red`). A declaration
/// carries no location, so it is reported at its enclosing rule's.
///
/// Values referencing `var()`/`env()`, a lone CSS-wide keyword, and `none` on
/// `text-shadow`/`box-shadow` are valid CSS lightningcss nonetheless leaves
/// unparsed, so they are exempt; the real properties it does not know are
/// listed in [`KNOWN_UNLISTED_PROPERTIES`].
///
/// The blocks of `@counter-style`, `@page` and `@viewport` hold descriptors
/// rather than properties, and lightningcss parses none of them, so an unknown
/// name there is measured against the enclosing rule's own descriptors instead
/// — a `@page` block holds real properties too, and a deprecated `@viewport`
/// block is not checked at all.
pub fn check_css(css: &str, filename: &str) -> Result<(), CssSyntaxError> {
    let warnings: Warnings = Default::default();

    let mut parsed_css = parse_stylesheet(css, filename, Some(warnings.clone()))
        .map_err(|e| syntax_error(e, filename))?;

    if let Some(warning) = warnings.read().ok().and_then(|w| w.first().cloned()) {
        return Err(syntax_error(warning, filename));
    }

    parsed_css.visit(&mut DeclarationCheck {
        filename,
        loc: None,
        context: DeclarationContext::Properties,
    })
}

#[derive(Clone)]
enum ObjUrlHolder {
    Computing,
    Computed(String),
}

#[cfg(target_arch = "wasm32")]
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

// On the web the asset bytes become a revocable blob Object URL. Off the web
// (e.g. the native render harness) there is no `URL` object, so the bytes are
// embedded directly as a base64-encoded `data:` URI instead.
#[cfg(target_arch = "wasm32")]
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

#[cfg(not(target_arch = "wasm32"))]
fn object_url_for(data: &[u8], mime_type: Option<&str>) -> Result<String, Error> {
    let mime = mime_type.unwrap_or("");
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(data)))
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
        #[cfg(not(feature = "sasso"))]
        if is_sass {
            return Err(NeedsSasso(filename.to_string()));
        }
        let needs_css_processing = is_sass || filename.ends_with(".css");

        if needs_css_processing {
            let css_source = {
                let mut css_source = String::new();
                file.read_to_string(&mut css_source).map_err(CantRead)?;
                drop(file); // Shouldn't be needed, IMO
                #[cfg(feature = "sasso")]
                if is_sass {
                    css_source = sasso::compile(&css_source, &sasso::Options::default())
                        .map_err(Error::CantParseSass)?;
                }
                css_source
            };
            let mut parsed_css = parse_stylesheet(&css_source, filename, None)
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

    #[cfg(target_arch = "wasm32")]
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
    // Blob Object URLs must be revoked on drop; the native `data:` URIs are
    // self-contained and need no cleanup, so this field only exists on the web.
    #[cfg(target_arch = "wasm32")]
    object_urls: Vec<String>,
}

impl AssetPack {
    pub fn new<R: Read + Seek>(reader: R, top_level: &str) -> Result<Self, Error> {
        use Error::*;

        let mut builder = Builder::new(ZipArchive::new(reader).map_err(CantReadZipFile)?);
        let data = builder.data(top_level)?.try_into().map_err(NotUtf8)?;
        Ok(Self {
            data,
            #[cfg(target_arch = "wasm32")]
            object_urls: builder.into_object_urls(),
        })
    }

    pub fn data(&self) -> &str {
        &self.data
    }
}

#[cfg(target_arch = "wasm32")]
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

    // The `.scss` entry path only exists with the `sasso` feature: the compiled
    // top level must have its SASS variable resolved and its nesting flattened.
    // A broken SCSS compile would leave `$brand` unresolved (or error outright),
    // so these assertions fail if `sasso::compile` stops being invoked.
    #[cfg(all(feature = "sasso", not(target_arch = "wasm32")))]
    #[test]
    fn compiles_scss_entry() -> Result<(), Box<dyn std::error::Error>> {
        let zip = Cursor::new(include_bytes!("../test-data/sass.zip"));
        let pack = AssetPack::new(zip, "sass.scss")?;
        let data = pack.data();
        assert!(
            data.contains("#123456"),
            "expected the SASS variable resolved to its value, got: {data}"
        );
        assert!(
            !data.contains("$brand"),
            "expected no unresolved SASS variable, got: {data}"
        );
        assert!(
            data.contains("body a"),
            "expected SASS nesting flattened to a descendant selector, got: {data}"
        );
        Ok(())
    }

    #[test]
    fn accepts_valid_css() -> Result<(), Box<dyn std::error::Error>> {
        check_css(
            "@import \"other.css\";\nbody a { color: #123456; background: url(bg.png); }\n",
            "style.css",
        )?;
        Ok(())
    }

    #[test]
    fn reports_located_syntax_errors() {
        for css in [
            "@media (min-width: ) {}",
            "a:: { color: red }",
            "@import url(x.css) foo bar;",
            "a[ { color: red }",
            "a { color: red !impotant }",
        ] {
            let error = check_css(css, "table.css").unwrap_err();

            assert_eq!("table.css", error.filename, "for {css}");
            assert!(error.line >= 1, "expected a 1-based line, got {error:?}");
            assert!(
                error.column >= 1,
                "expected a 1-based column, got {error:?}"
            );
            assert!(!error.message.is_empty(), "for {css}");
        }
    }

    #[test]
    fn reports_one_based_lines() {
        let error =
            check_css("a { color: red }\n\n@media (min-width: ) {}\n", "table.css").unwrap_err();

        assert_eq!(3, error.line);
        assert_eq!(18, error.column);
    }

    // An unknown at-rule does not stop the parse; the check collects it anyway.
    #[test]
    fn reports_errors_the_parser_recovers_from() {
        let error = check_css("a { color: red }\n@tailwind base;\n", "table.css").unwrap_err();

        assert_eq!("table.css", error.filename);
        assert_eq!(2, error.line);
        assert!(!error.message.is_empty());
    }

    const UNKNOWN_COLR: &str = "unknown property `colr` (a real property \
        lightningcss does not know? add it to KNOWN_UNLISTED_PROPERTIES)";

    // What the declaration check does and does not report, declaration by
    // declaration: `None` expects the CSS to pass, `Some` the exact message.
    // A value lightningcss cannot parse is a typo unless a `var()`/`env()`
    // reference, a CSS-wide keyword, or a shadow `none` explains it, and an
    // unknown property name is a typo unless KNOWN_UNLISTED_PROPERTIES lists
    // it — `text-wrap` is real CSS this lightningcss lacks, and is reported
    // until someone adds it there. An unterminated block is closed at end of
    // input, and a stray `;` is no declaration at all.
    #[test]
    fn reports_declaration_typos() -> Result<(), Box<dyn std::error::Error>> {
        for (css, expected) in [
            ("a { colr: red }", Some(UNKNOWN_COLR.to_string())),
            (
                "a { text-wrap: balance }",
                Some(UNKNOWN_COLR.replace("colr", "text-wrap")),
            ),
            (
                "a { width: 10pxx }",
                Some("unparseable value for `width`: `10pxx`".to_string()),
            ),
            (
                "a { color: redd }",
                Some("unparseable value for `color`: `redd`".to_string()),
            ),
            (
                "a { color: }",
                Some("unparseable value for `color`: ` `".to_string()),
            ),
            (
                "a { margin: 1px 2px 3px 4px 5px }",
                Some("unparseable value for `margin`: `1px 2px 3px 4px 5px`".to_string()),
            ),
            (
                "a { transition: color .2s eas }",
                Some("unparseable value for `transition`: `color .2s eas`".to_string()),
            ),
            (
                "a { display: flexx }",
                Some("unparseable value for `display`: `flexx`".to_string()),
            ),
            ("a { --x: 1px; width: var(--x) }", None),
            ("a { width: calc(100% - var(--x)) }", None),
            ("a { padding-top: env(safe-area-inset-top) }", None),
            ("a { color: rgb(0 0 0 / var(--alpha)) }", None),
            ("a { container-type: inline-size }", None),
            ("a { color: red; ; }", None),
            ("a { background: url( }", None),
            ("a { color: red", None),
            ("a { text-shadow: none }", None),
            ("a { box-shadow: none }", None),
            ("a { transition: none }", None),
            ("a { color: inherit !important }", None),
        ] {
            match (check_css(css, "table.css"), expected) {
                (Ok(()), None) => {}
                (Err(error), Some(message)) if error.message == message => {}
                (got, expected) => {
                    return Err(format!("{css}: expected {expected:?}, got {got:?}").into());
                }
            }
        }
        Ok(())
    }

    // `!important` declarations are kept in their own list, which the check
    // must visit too.
    #[test]
    fn reports_typos_in_important_declarations() -> Result<(), Box<dyn std::error::Error>> {
        let error = check_css("a { color: red; colr: red !important }", "table.css").unwrap_err();

        assert_eq!(UNKNOWN_COLR, error.message);
        Ok(())
    }

    // A declaration carries no location, so it is reported at its enclosing
    // rule's — the innermost one, not the at-rule wrapping it.
    #[test]
    fn declaration_typos_report_the_enclosing_rule() -> Result<(), Box<dyn std::error::Error>> {
        let error = check_css("a { color: red }\n\nb { width: 10pxx }\n", "table.css").unwrap_err();
        assert_eq!(3, error.line);
        assert_eq!(1, error.column);

        let nested = check_css(
            "@media (min-width: 1px) {\n  a { width: 10pxx }\n}\n",
            "table.css",
        )
        .unwrap_err();
        assert_eq!(2, nested.line);
        assert_eq!("table.css", nested.filename);
        Ok(())
    }

    // `@counter-style`, `@page` and `@viewport` hold descriptors rather than
    // properties, so a name in one of those blocks is measured against that
    // rule's own descriptors — whatever its value, since lightningcss parses
    // none of them. A `@page` block holds real properties too, `@viewport` is
    // deprecated and skipped, and `@position-try` holds nothing but ordinary
    // properties. The context is the innermost rule's, so a block following a
    // descriptor rule is checked as properties again.
    #[test]
    fn checks_descriptor_blocks_against_their_rules() -> Result<(), Box<dyn std::error::Error>> {
        for (css, expected) in [
            (
                "@counter-style c { system: cyclic; symbols: a; suffix: \". \" }",
                None,
            ),
            (
                "@counter-style c { systm: cyclic }",
                Some("unknown @counter-style descriptor `systm`".to_string()),
            ),
            ("@page { margin: 1in; size: a4 }", None),
            (
                "@page { margn: 1in }",
                Some(UNKNOWN_COLR.replace("colr", "margn")),
            ),
            ("@position-try --p { top: 0; position-area: top }", None),
            (
                "@position-try --p { widthh: 0 }",
                Some(UNKNOWN_COLR.replace("colr", "widthh")),
            ),
            ("@viewport { width: device-width }", None),
            (
                "@media print { @counter-style c { system: cyclic } a { colr: red } }",
                Some(UNKNOWN_COLR.to_string()),
            ),
        ] {
            match (check_css(css, "table.css"), expected) {
                (Ok(()), None) => {}
                (Err(error), Some(message)) if error.message == message => {}
                (got, expected) => {
                    return Err(format!("{css}: expected {expected:?}, got {got:?}").into());
                }
            }
        }
        Ok(())
    }

    // A `@page` margin box is a rule lightningcss does not expose as a
    // `CssRule`, so its typo is reported at the `@page` enclosing it, while
    // `@layer` and `@position-try` carry a location like any other rule.
    #[test]
    fn descriptor_rules_are_located() -> Result<(), Box<dyn std::error::Error>> {
        let margin_box = check_css(
            "a { color: red }\n@page {\n  margin: 1in;\n  @top-center { contnt: \"x\" }\n}\n",
            "table.css",
        )
        .unwrap_err();
        assert_eq!(2, margin_box.line);
        assert_eq!(UNKNOWN_COLR.replace("colr", "contnt"), margin_box.message);

        let position_try = check_css("@position-try --p { widthh: 0 }", "table.css").unwrap_err();
        assert_eq!(1, position_try.line);
        assert_eq!(1, position_try.column);

        let layered = check_css("@layer a, b;\nb { width: 10pxx }", "table.css").unwrap_err();
        assert_eq!(2, layered.line);
        Ok(())
    }

    #[test]
    fn exemptions_recognize_their_token_shapes() -> Result<(), Box<dyn std::error::Error>> {
        use lightningcss::traits::ParseWithOptions;

        fn tokens(value: &'static str) -> Result<TokenList<'static>, String> {
            TokenList::parse_string_with_options(value, ParserOptions::default())
                .map_err(|e| format!("can't parse {value}: {e:?}"))
        }

        assert!(contains_var_or_env(&tokens("var(--x)")?));
        assert!(contains_var_or_env(&tokens("env(safe-area-inset-top)")?));
        assert!(contains_var_or_env(&tokens("foo(1px, var(--x))")?));
        assert!(contains_var_or_env(&tokens("var(--x, var(--y))")?));
        assert!(contains_var_or_env(&tokens("rgb(0 0 0 / var(--alpha))")?));
        assert!(contains_var_or_env(&tokens("light-dark(var(--l), black)")?));
        assert!(!contains_var_or_env(&tokens("1px solid red")?));

        assert!(is_css_wide_keyword(&tokens("inherit")?));
        assert!(is_css_wide_keyword(&tokens(" revert-layer ")?));
        assert!(is_css_wide_keyword(&tokens("INITIAL")?));
        assert!(!is_css_wide_keyword(&tokens("inherit red")?));
        assert!(!is_css_wide_keyword(&tokens("inherited")?));

        assert!(is_shadow_none("box-shadow", &tokens("none")?));
        assert!(is_shadow_none("text-shadow", &tokens("none")?));
        assert!(!is_shadow_none("color", &tokens("none")?));
        assert!(!is_shadow_none("box-shadow", &tokens("none none")?));
        Ok(())
    }

    // The gate for the declaration check: every stylesheet this tree ships
    // must pass it, or `render` and `live` reject CSS that is perfectly good.
    #[cfg(all(feature = "sasso", not(target_arch = "wasm32")))]
    #[test]
    fn accepts_the_stylesheets_of_the_tree_it_is_vendored_in()
    -> Result<(), Box<dyn std::error::Error>> {
        let web = concat!(env!("CARGO_MANIFEST_DIR"), "/../front-ends/web");

        for name in ["mb2.scss", "table.scss"] {
            let path = format!("{web}/{name}");
            let source = std::fs::read_to_string(&path)?;
            let importer = sasso::FsImporter::new(Vec::new());
            let options = sasso::Options::default()
                .with_syntax(sasso::Syntax::Scss)
                .with_importer(&importer)
                .with_url(&path);
            let compiled = sasso::compile(&source, &options)
                .map_err(|e| format!("can't compile {path}: {e:?}"))?;
            check_css(&compiled, &format!("{name} (compiled)"))?;
        }

        for name in ["static/mb2-static.css", "static/spinner.css"] {
            check_css(&std::fs::read_to_string(format!("{web}/{name}"))?, name)?;
        }
        Ok(())
    }

    // Native (non-browser) build: assets are embedded as base64 `data:` URIs
    // rather than blob Object URLs, so the compiled top level rewrites its
    // `@import` (and the nested `url()`) to self-contained data URIs.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn embeds_assets_as_data_uris() -> Result<(), Box<dyn std::error::Error>> {
        let zip = Cursor::new(include_bytes!("../test-data/alternate.zip"));
        let pack = AssetPack::new(zip, "alternate.css")?;
        let data = pack.data();
        assert!(
            data.starts_with("@import \"data:"),
            "expected @import rewritten to a data: URI, got: {data}"
        );
        assert!(
            data.contains(";base64,"),
            "expected a base64-encoded data: URI, got: {data}"
        );
        assert!(
            !data.contains("blob:"),
            "native path must not emit blob: Object URLs, got: {data}"
        );
        Ok(())
    }
}
