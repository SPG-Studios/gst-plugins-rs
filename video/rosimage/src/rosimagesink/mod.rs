use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct RosImageSink(ObjectSubclass<imp::RosImageSink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

// Registers the type for our element, and then registers in GStreamer under
// the name "rosimagesink" for being able to instantiate it via e.g.
// gst::ElementFactory::make().
pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rosimagesink",
        gst::Rank::None,
        RosImageSink::static_type(),
    )
}
