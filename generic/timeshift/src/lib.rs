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
use gst::glib;

mod timeshift;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    timeshift::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    timeshift,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
