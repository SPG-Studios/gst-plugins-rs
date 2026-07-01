// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// GL deferred-label compositor: the GL-native counterpart to `overlaycompositor`,
// placing deferred labels and rendering them on GL textures via skia GPU. Gated
// behind the `gl` cargo feature.

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OverlayCompositorGl(ObjectSubclass<imp::OverlayCompositorGl>)
        @extends gst_gl::GLFilter, gst_gl::GLBaseFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "overlaycompositorgl",
        gst::Rank::NONE,
        OverlayCompositorGl::static_type(),
    )
}
