// Copyright (C) 2026 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
#![allow(clippy::non_send_fields_in_send_ty, unused_doc_comments)]

/**
 * plugin-videorate:
 *
 * Since: plugins-rs-0.12.0
*/
use gst::glib;

mod videorate2;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    videorate2::register(plugin)
}

gst::plugin_define!(
    videorate2,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
