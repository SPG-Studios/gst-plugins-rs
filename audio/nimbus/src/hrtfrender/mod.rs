use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct NimbusHrtf(ObjectSubclass<imp::NimbusHrtf>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "audionimbushrtfrender",
        gst::Rank::NONE,
        NimbusHrtf::static_type(),
    )?;

    Ok(())
}
