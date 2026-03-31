use {
    css_asset_pack::AssetPack,
    gloo_events::EventListener,
    gloo_utils::document,
    js_sys::Uint8Array,
    log::{error, info},
    std::io::Cursor,
    wasm_bindgen::JsCast,
    wasm_bindgen_futures::JsFuture,
    web_sys::{CharacterData, File, HtmlInputElement, XPathResult},
    yew::{html::Scope, platform::spawn_local, prelude::*},
};

pub(super) enum Msg {
    Upload,
    Apply(File),
    SavePack(AssetPack),
}

#[derive(Default)]
pub(super) struct Skinner {
    change_listener: Option<EventListener>,
    asset_pack: Option<AssetPack>,
}

impl Component for Skinner {
    type Message = Msg;
    type Properties = ();

    fn create(_: &yew::Context<Self>) -> Self {
        Default::default()
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        use Msg::*;

        match msg {
            Upload => self.upload(ctx.link()),
            Apply(file) => {
                let link = ctx.link().clone();
                spawn_local(async { Self::apply(file, link).await });
                false
            }
            SavePack(pack) => {
                self.asset_pack = Some(pack);
                false
            }
        }
    }

    fn view(&self, ctx: &yew::Context<Self>) -> Html {
        let onclick = ctx.link().callback(|_| Msg::Upload);
        html! {
            <button id="skin" {onclick}> { "Skin" } </button>
        }
    }
}

impl Skinner {
    pub fn upload(&mut self, link: &Scope<Self>) -> bool {
        let link = link.clone();
        let element = match document().create_element("input") {
            Ok(element) => element,
            Err(e) => {
                error!("Could not create input element: {e:?}");
                return false;
            }
        };
        let input = match element.dyn_into::<HtmlInputElement>() {
            Ok(input) => input,
            Err(input) => {
                error!("Could not turn {input:?} into HtmlInputElement");
                return false;
            }
        };
        if let Err(e) = input.set_attribute("type", "file") {
            error!("Could not set {input:?}'s type to file: {e:?}");
            return false;
        }
        if let Err(e) = input.set_attribute(
            "accept",
            ".zip,application/zip,application/x-zip-compressed",
        ) {
            error!("Could not set {input:?}'s accept: {e:?}");
            return false;
        }
        // NOTE: don't bother setting change_listener back to None,
        // after the handler has been triggered, because there's not
        // much of a leak if we leave it in place.  After all, if we
        // create a new listener, it'll overwrite--and hence drop--the
        // old one, so at most we waste the space of one unneeded
        // listener.
        self.change_listener = Some(EventListener::once(
            &input,
            "change",
            move |e: &Event| match e.target() {
                None => error!("{e:?} has no target"),
                Some(target) => match target.dyn_into::<HtmlInputElement>() {
                    Err(target) => error!("Could not change {target:?} into HtmlInputElement"),
                    Ok(input) => match input.files() {
                        None => info!("No files"),
                        Some(files) => {
                            if let Some(file) = files.get(0) {
                                link.send_message(Msg::Apply(file));
                            } else {
                                info!("No file selected");
                            }
                        }
                    },
                },
            },
        ));
        input.click();
        false
    }

    pub async fn apply(file: File, link: Scope<Self>) {
        match JsFuture::from(file.bytes()).await {
            Err(e) => {
                error!("Could not get file bytes: {e:?}");
            }
            Ok(bytes) => {
                let u8_array = Uint8Array::new(&bytes);
                let bytes: Vec<u8> = u8_array.to_vec();
                let zip = Cursor::new(bytes);
                match AssetPack::new(zip, "styles.css") {
                    Err(e) => error!("could not create Asset Pack: {e:?}"),
                    Ok(pack) => {
                        if let Some(css) = css() {
                            css.set_data(pack.data());
                        } else {
                            make_css(&pack);
                        }
                        link.send_message(Msg::SavePack(pack));
                    }
                }
            }
        }
    }
}

fn css() -> Option<CharacterData> {
    let document = document();
    document
        .evaluate_with_opt_callback_and_type(
            "//html/head/style[1]/text()",
            &document,
            None,
            XPathResult::FIRST_ORDERED_NODE_TYPE,
        )
        .ok()
        .and_then(|result| {
            result.single_node_value().ok().and_then(|result| {
                result.and_then(|result| result.dyn_into::<CharacterData>().ok())
            })
        })
}

fn make_css(pack: &AssetPack) {
    let document = document();

    let path_result = match document.evaluate_with_opt_callback_and_type(
        "//html/head/link[@rel=\"stylesheet\"]",
        &document,
        None,
        XPathResult::FIRST_ORDERED_NODE_TYPE,
    ) {
        Err(e) => {
            error!("Could not find stylesheet link: {e:?}");
            return;
        }
        Ok(path_result) => path_result,
    };

    let link_node = match path_result.single_node_value() {
        Err(e) => {
            error!("Could not get the single node value: {e:?}");
            return;
        }
        Ok(None) => {
            error!("Single node value was empty");
            return;
        }
        Ok(Some(link_node)) => link_node,
    };

    let style = match document.create_element("style") {
        Err(e) => {
            error!("Could not create style: {e:?}");
            return;
        }
        Ok(style) => style,
    };

    let link_parent = match link_node.parent_node() {
        None => {
            error!("Could not get link's parent");
            return;
        }
        Some(link_parent) => link_parent,
    };

    let link_next_sibling = match link_node.next_sibling() {
        None => {
            error!("link has no sibling");
            return;
        }
        Some(link_next_sibling) => link_next_sibling,
    };

    if let Err(e) = link_parent.insert_before(&style, Some(&link_next_sibling)) {
        error!("Could not insert style: {e:?}");
        return;
    }

    style.set_text_content(Some(pack.data()));
}
