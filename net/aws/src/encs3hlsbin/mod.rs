// Copyright (C) 2021, Daily
//      Author: Rajneesh Soni <rajneesh@daily.co>
//      Author: Arun Raghavan <arun@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![deny(clippy::unwrap_used)]
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct EncS3HlsBin(ObjectSubclass<imp::EncS3HlsBin>) @extends gst::Bin, gst::Element, gst::Object;
}

unsafe impl Send for EncS3HlsBin {}
unsafe impl Sync for EncS3HlsBin {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "encs3hlsbin",
        gst::Rank::None,
        EncS3HlsBin::static_type(),
    )
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    encs3hlsbin,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY")
);
