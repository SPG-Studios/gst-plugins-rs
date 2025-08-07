mod events;
mod imp;
use gst::glib;
use gst::prelude::*;

glib::wrapper! {
    pub struct OpenAI(ObjectSubclass<imp::OpenAI>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "openai",
        gst::Rank::NONE,
        OpenAI::static_type(),
    )
}
