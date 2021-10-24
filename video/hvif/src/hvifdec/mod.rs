// Copyright (c) 2021 Emmanuel Gil Peyrot <linkmauve@linkmauve.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct HvifDecoder(ObjectSubclass<imp::HvifDec>) @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

// GStreamer elements need to be thread-safe. For the private implementation this is automatically
// enforced but for the public wrapper type we need to specify this manually.
unsafe impl Send for HvifDecoder {}
unsafe impl Sync for HvifDecoder {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "hvifdec",
        gst::Rank::Primary,
        HvifDecoder::static_type(),
    )
}
