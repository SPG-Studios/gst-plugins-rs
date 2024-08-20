// Copyright (C) 2023 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
/**
 * element-qoadec:
 * @short_description: Decode audio encoded in QOA format.
 *
 * Decoder for the Quite OK Audio format. Supports file and streaming modes.
 *
 * ## Example pipeline
 * ```bash
 * gst-launch-1.0 filesrc location=audio.qoa ! qoadec ! autoaudiosink
 * ```
 *
 * Since: plugins-rs-0.11.0-alpha.1
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct QoaDec(ObjectSubclass<imp::QoaDec>) @extends gst_audio::AudioDecoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "qoadec",
        gst::Rank::Marginal,
        QoaDec::static_type(),
    )
}
