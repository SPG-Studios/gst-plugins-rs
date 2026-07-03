// Copyright (C) 2026 Lluc Simó Margalef <lsimmar@upv.es>, Immersive
//    Interactive Media (IIM) R&D group at Universitat Politècnica de València.
//
// This plugin has been developed with support by the following projects:
// CIAICO/2022/025, from Conselleria de Innovación, Universidades, Ciencia y
// Sociedad Digital of the GVA (DOGV 8919/05.10.2020); and grant
// PID2021-126645OB-I00, funded by MICIU/AEI/10.13039/501100011033/ and by "ERDF
// A way of making Europe".
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
use glib::prelude::*;
use gst::glib;

mod imp;

glib::wrapper! {
    pub struct Timeshift(ObjectSubclass<imp::Timeshift>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "timeshift",
        gst::Rank::NONE,
        Timeshift::static_type(),
    )
}
