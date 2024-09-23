// Copyright (C) 2025, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * element-moqdemux:
 * @short-description: Media over QUIC track handling
 *
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct MoqDemux(ObjectSubclass<imp::MoqDemux>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "moqdemux",
        gst::Rank::NONE,
        MoqDemux::static_type(),
    )
}
