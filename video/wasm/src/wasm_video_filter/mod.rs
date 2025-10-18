use gst::glib;
use gst::prelude::StaticType;

mod imp;

glib::wrapper! {
    pub struct WasmVideoFilter(ObjectSubclass<imp::WasmVideoFilter>) @extends gst_base::BaseTransform, gst_video::VideoFilter, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "wasmvideofilter",
        gst::Rank::NONE,
        WasmVideoFilter::static_type(),
    )
}
