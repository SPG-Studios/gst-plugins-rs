// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// A GstGLFilter-based object-detection overlay that renders with skia's GPU
// (Ganesh) backend directly onto the pipeline's GL textures. The GL-native
// counterpart to `odoverlay`; gated behind the `gl` cargo feature.

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct ObjectDetectionOverlayGl(ObjectSubclass<imp::ObjectDetectionOverlayGl>)
        @extends gst_gl::GLFilter, gst_gl::GLBaseFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "odoverlaygl",
        gst::Rank::NONE,
        ObjectDetectionOverlayGl::static_type(),
    )
}
