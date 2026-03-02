// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

use crate::utils::Format;

glib::wrapper! {
    pub struct Encoder(ObjectSubclass<imp::Encoder>) @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    Format::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Element::register(
        Some(plugin),
        "imagersenc",
        gst::Rank::SECONDARY + 1,
        Encoder::static_type(),
    )
}
