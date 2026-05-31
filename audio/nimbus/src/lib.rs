use gst::glib;

mod hrtfrender;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    hrtfrender::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    audionimbus,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
