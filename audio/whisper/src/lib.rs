// Copyright (C) 2025 <diego.nieto.m@outlook.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;

mod transcriber;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    transcriber::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    whisper,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", "commit-id"),
    "MPL/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
