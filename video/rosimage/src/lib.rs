use gst::glib;

mod rosimagesink;
mod rosimagesrc;

gst::plugin_define!(
    rosimagebridge,
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
    rosimagesrc::register(plugin)?;
    rosimagesink::register(plugin)?;
    Ok(())
}

rosrust::rosmsg_include!(sensor_msgs / Image);
