use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct RosImageSrc(ObjectSubclass<imp::RosImageSrc>) @extends gst_base::BaseSrc, gst::Element, gst::Object;
}

// Registers the type for our element, and then registers in GStreamer under
// the name "rosimagesrc" for being able to instantiate it via e.g.
// gst::ElementFactory::make().
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rosimagesrc",
        gst::Rank::None,
        RosImageSrc::static_type(),
    )
}
