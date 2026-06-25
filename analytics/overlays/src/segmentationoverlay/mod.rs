// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

// pub(crate) so the GL element (segmentationoverlaygl) can reuse the mask
// colorization (`segmentation_mask_layers`) and composite the same layers.
pub(crate) mod masks;

glib::wrapper! {
    pub struct SegmentationOverlay(ObjectSubclass<imp::SegmentationOverlay>) @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "segoverlay",
        gst::Rank::NONE,
        SegmentationOverlay::static_type(),
    )
}
