// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
// Copyright (C) 2026 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

mod imp;
#[allow(unused)]
#[allow(unexpected_cfgs)]
mod voxtral_mini_realtime;

glib::wrapper! {
    pub struct Transcriber(ObjectSubclass<imp::Transcriber>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "voxtral-mini-realtime-transcriber",
        gst::Rank::NONE,
        Transcriber::static_type(),
    )
}
