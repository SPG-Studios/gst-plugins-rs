//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

pub mod imp;

#[cfg(test)]
mod tests;

glib::wrapper! {
    pub struct RtpH266Pay(ObjectSubclass<imp::RtpH266Pay>)
        @extends crate::basepay::RtpBasePay2, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        AggregateMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }

    gst::Element::register(
        Some(plugin),
        "rtph266pay",
        gst::Rank::PRIMARY,
        RtpH266Pay::static_type(),
    )
}

/// Controls how NAL units of an access unit are aggregated into RTP packets.
#[derive(Copy, Clone, Debug, PartialEq, Eq, glib::Enum, Default)]
#[enum_type(name = "GstRtpH266PayAggregateMode")]
#[repr(i32)]
pub enum AggregateMode {
    /// Do not aggregate: each NAL unit goes into its own RTP packet.
    #[default]
    #[enum_value(name = "Do not aggregate NAL units", nick = "none")]
    None,
    /// Aggregate the non-VCL NAL units (parameter sets, SEI, ...) ahead of a
    /// VCL NAL unit into one AP, without adding latency.
    #[enum_value(
        name = "Aggregate non-VCL NAL units (no latency added)",
        nick = "zero-latency"
    )]
    ZeroLatency,
    /// Aggregate as many NAL units as fit the MTU. With one access unit per
    /// input buffer this behaves like zero-latency, additionally bundling
    /// small VCL NAL units.
    #[enum_value(name = "Aggregate as many NAL units as the MTU allows", nick = "max")]
    Max,
}
