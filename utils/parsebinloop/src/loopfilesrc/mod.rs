// Copyright (C) 2025 Axel Tobieson <axel.tobieson@spiideo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};

mod imp;

glib::wrapper! {
    pub struct LoopFileSrc(ObjectSubclass<imp::LoopFileSrc>)
    @extends gst::Bin, gst::Element, gst::Object, @implements gst::URIHandler;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "loopfilesrc",
        gst::Rank::NONE,
        LoopFileSrc::static_type(),
    )
}
