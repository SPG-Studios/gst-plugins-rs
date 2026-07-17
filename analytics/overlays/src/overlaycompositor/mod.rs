// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Deferred-label compositor: places and renders the labels that upstream overlay
// elements deferred (see `overlay_intent`), relocating them globally by priority.

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OverlayCompositor(ObjectSubclass<imp::OverlayCompositor>) @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "overlaycompositor",
        gst::Rank::NONE,
        OverlayCompositor::static_type(),
    )
}
