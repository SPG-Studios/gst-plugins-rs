// Copyright (C) 2022, Daily
//      Author: Arun Raghavan <arun@asymptotic.io>
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
// Copyright (C) 2025, GlobalM SA
//      Author: Mart Raudsepp <mart.raudsepp@globalm.media>
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
    pub struct S3HlsMultivariantSink(ObjectSubclass<imp::S3HlsMultivariantSink>) @extends gst::Bin, gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub(crate) struct S3HlsMultivariantSinkPad(ObjectSubclass<imp::S3HlsMultivariantSinkPad>) @extends gst::GhostPad, gst::ProxyPad, gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "awss3hlsmultivariantsink",
        gst::Rank::NONE,
        S3HlsMultivariantSink::static_type(),
    )
}
