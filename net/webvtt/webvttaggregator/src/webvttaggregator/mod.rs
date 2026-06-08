use gst::glib;
use gst::prelude::*;
mod imp;

glib::wrapper! {
    pub(crate) struct WebVTTAggregatorPad(ObjectSubclass<imp::WebVTTAggregatorPad>) @extends gst_base::AggregatorPad, gst::Pad, gst::Object;
}

glib::wrapper! {
    pub struct WebVTTAggregator(ObjectSubclass<imp::WebVTTAggregator>) @extends gst_base::Aggregator, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "webvttaggregator",
        gst::Rank::NONE,
        WebVTTAggregator::static_type(),
    )
}
