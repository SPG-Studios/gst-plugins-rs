// Copyright (C) 2023 Rafael Caricio <rafael@caricio.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
/**
 * element-qoaparse:
 * @short_description: Parser for audio encoded in QOA format.
 *
 * Parser for the Quite OK Audio format. Supports file and streaming modes.
 *
 * ## Example pipeline
 * ```bash
 * gst-launch-1.0 filesrc location=audio.qoa ! qoaparse ! qoadec ! autoaudiosink
 * ```
 *
 * Since: plugins-rs-0.11.0-alpha.1
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct QoaParse(ObjectSubclass<imp::QoaParse>) @extends gst_base::BaseParse, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "qoaparse",
        gst::Rank::Primary,
        QoaParse::static_type(),
    )
}
