use gst::glib;
use gst::prelude::StaticType;

mod imp;

glib::wrapper! {
    pub struct WasmFilter(ObjectSubclass<imp::WasmFilter>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "wasmfilter",
        gst::Rank::NONE,
        WasmFilter::static_type(),
    )
}
