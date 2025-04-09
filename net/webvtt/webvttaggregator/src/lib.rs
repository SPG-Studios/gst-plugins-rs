use gst::glib;
pub mod webvttaggregator;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    webvttaggregator::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    webvttsink_gst_plugin,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Proprietary",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
