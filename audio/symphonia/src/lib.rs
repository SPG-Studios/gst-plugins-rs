// Copyright (C) 2022-2026 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;

mod common;

#[cfg(feature = "flac")]
mod flac;

#[cfg(feature = "mp3")]
mod mp3;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "flac")]
    flac::register(plugin)?;
    #[cfg(feature = "mp3")]
    mp3::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    symphonia,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
