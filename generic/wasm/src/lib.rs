use gst::glib;

mod json_caps;
mod wasm_filter;

gst::plugin_define!(
    wasm,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    wasm_filter::register(plugin)?;
    Ok(())
}
