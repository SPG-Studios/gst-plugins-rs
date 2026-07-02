// GStreamer RTP MPEG-4 part 2 Video Elementary Stream Payloader
//
// Copyright (C) 2023 Tim-Philipp Müller <tim centricular com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod mpeg4_video;

pub mod imp;

glib::wrapper! {
    pub struct RtpMpeg4VideoPay(ObjectSubclass<imp::RtpMpeg4VideoPay>)
        @extends crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "rtpmp4vpay2",
        gst::Rank::PRIMARY + 1, // higher rank than rtpmp4gpay2 since mp4v is supported more widely
        RtpMpeg4VideoPay::static_type(),
    )
}
