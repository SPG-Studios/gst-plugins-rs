// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct MoqMux(ObjectSubclass<imp::MoqMux>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct MoqMuxSinkPad(ObjectSubclass<imp::MoqMuxSinkPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "moqmux",
        gst::Rank::NONE,
        MoqMux::static_type(),
    )
}
