# Skinnable Tauri Demo

This is a demo of css-asset-pack.

This is the output of "cargo create-tauri-app" with
[Yew](https://yew.rs) chosen for the front-end. It's been modified to
have a "Skin" component that is a button that does a client-side
upload of a zip file that can change the appearance of the Tauri + Yew
window.


## Usage

Install the pre-requisites and then invoke `cargo tauri dev`. Click on
the `Skin` button and choose on of the files in the skins
directory. None of the skins are remotely interesting, but they do
test `@import`, `@font-family`, and background urls.
