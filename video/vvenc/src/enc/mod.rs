// Copyright (C) 2025 Carlos Bentzen <cadubentzen@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

/// Preset enum represents different encoding presets for the VVenC encoder.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstVVenCSpeedPreset")]
pub enum SpeedPreset {
    #[enum_value(name = "Faster encoding", nick = "faster")]
    Faster = 0,
    #[enum_value(name = "Fast encoding", nick = "fast")]
    Fast = 1,
    #[default]
    #[enum_value(name = "Medium encoding", nick = "medium")]
    Medium = 2,
    #[enum_value(name = "Slow encoding", nick = "slow")]
    Slow = 3,
    #[enum_value(name = "Slower encoding", nick = "slower")]
    Slower = 4,
    #[enum_value(name = "Medium low decoding energy", nick = "medium-low-dec-nrg")]
    MediumLowDecNrg = 5,
    #[enum_value(name = "First pass encoding", nick = "first-pass")]
    FirstPass = 6,
    #[enum_value(name = "Tool test encoding", nick = "tool-test")]
    ToolTest = 7,
}

/// Profile enum represents different encoding profiles for the VVenC encoder.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstVVenCProfile")]
pub enum Profile {
    #[default]
    #[enum_value(name = "Auto profile", nick = "auto")]
    Auto = 0,
    #[enum_value(name = "Main 10 profile", nick = "main10")]
    Main10 = 1,
    #[enum_value(name = "Main 10 still picture profile", nick = "main10-still-picture")]
    Main10StillPicture = 2,
    #[enum_value(name = "Main 10 4:4:4 profile", nick = "main10-444")]
    Main10444 = 3,
    #[enum_value(
        name = "Main 10 4:4:4 still picture profile",
        nick = "main10-444-still-picture"
    )]
    Main10444StillPicture = 4,
    #[enum_value(name = "Multilayer main 10 profile", nick = "multilayer-main10")]
    MultilayerMain10 = 5,
    #[enum_value(
        name = "Multilayer main 10 still picture profile",
        nick = "multilayer-main10-still-picture"
    )]
    MultilayerMain10StillPicture = 6,
    #[enum_value(
        name = "Multilayer main 10 4:4:4 profile",
        nick = "multilayer-main10-444"
    )]
    MultilayerMain10444 = 7,
    #[enum_value(
        name = "Multilayer main 10 4:4:4 still picture profile",
        nick = "multilayer-main10-444-still-picture"
    )]
    MultilayerMain10444StillPicture = 8,
}

/// Tier enum represents different encoding tiers for the VVenC encoder.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstVVenCTier")]
pub enum Tier {
    #[default]
    #[enum_value(name = "Main tier", nick = "main")]
    Main = 0,
    #[enum_value(name = "High tier", nick = "high")]
    High = 1,
}

/// Level enum represents different encoding levels for the VVenC encoder.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstVVenCLevel")]
pub enum Level {
    #[default]
    #[enum_value(name = "Auto level", nick = "auto")]
    Auto = 0,
    #[enum_value(name = "Level 1", nick = "level1")]
    Level1 = 1,
    #[enum_value(name = "Level 2", nick = "level2")]
    Level2 = 2,
    #[enum_value(name = "Level 2.1", nick = "level2-1")]
    Level2_1 = 3,
    #[enum_value(name = "Level 3", nick = "level3")]
    Level3 = 4,
    #[enum_value(name = "Level 3.1", nick = "level3-1")]
    Level3_1 = 5,
    #[enum_value(name = "Level 4", nick = "level4")]
    Level4 = 6,
    #[enum_value(name = "Level 4.1", nick = "level4-1")]
    Level4_1 = 7,
    #[enum_value(name = "Level 5", nick = "level5")]
    Level5 = 8,
    #[enum_value(name = "Level 5.1", nick = "level5-1")]
    Level5_1 = 9,
    #[enum_value(name = "Level 5.2", nick = "level5-2")]
    Level5_2 = 10,
    #[enum_value(name = "Level 6", nick = "level6")]
    Level6 = 11,
    #[enum_value(name = "Level 6.1", nick = "level6-1")]
    Level6_1 = 12,
    #[enum_value(name = "Level 6.2", nick = "level6-2")]
    Level6_2 = 13,
    #[enum_value(name = "Level 6.3", nick = "level6-3")]
    Level6_3 = 14,
    #[enum_value(name = "Level 15.5", nick = "level15-5")]
    Level15_5 = 15,
}

/// DecodingRefreshType enum represents different decoding refresh types for the VVenC encoder.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstVVenCDecodingRefreshType")]
pub enum DecodingRefreshType {
    #[enum_value(name = "No refresh", nick = "none")]
    None = 0,
    #[default]
    #[enum_value(name = "CRA refresh", nick = "cra")]
    Cra = 1,
    #[enum_value(name = "IDR refresh", nick = "idr")]
    Idr = 2,
    #[enum_value(name = "Recovery point SEI refresh", nick = "recovery-point-sei")]
    RecoveryPointSei = 3,
    #[enum_value(name = "CRA CRE refresh", nick = "cra-cre")]
    CraCre = 4,
    #[enum_value(name = "IDR no RADL refresh", nick = "idr-no-radl")]
    IdrNoRadl = 5,
}

/// HdrMode enum represents different HDR modes for the VVenC encoder.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstVVenCHdrMode")]
pub enum HdrMode {
    #[default]
    #[enum_value(name = "HDR mode off", nick = "off")]
    Off = 0,
    #[enum_value(name = "PQ HDR mode", nick = "pq")]
    Pq = 1,
    #[enum_value(name = "HLG HDR mode", nick = "hlg")]
    Hlg = 2,
    #[enum_value(name = "PQ BT.2020 HDR mode", nick = "pq-bt2020")]
    PqBt2020 = 3,
    #[enum_value(name = "HLG BT.2020 HDR mode", nick = "hlg-bt2020")]
    HlgBt2020 = 4,
    #[enum_value(name = "User defined HDR mode", nick = "user-defined")]
    UserDefined = 5,
    #[enum_value(name = "SDR BT.709 HDR mode", nick = "sdr-bt709")]
    SdrBt709 = 6,
    #[enum_value(name = "SDR BT.2020 HDR mode", nick = "sdr-bt2020")]
    SdrBt2020 = 7,
    #[enum_value(name = "SDR BT.470BG HDR mode", nick = "sdr-bt470bg")]
    SdrBt470bg = 8,
}

#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(i32)]
#[enum_type(name = "GstVVenCCurrentPass")]
pub enum CurrentPass {
    #[default]
    #[enum_value(name = "Single pass", nick = "single")]
    Single = -1,
    #[enum_value(name = "First pass", nick = "first-pass")]
    First = 0,
    #[enum_value(name = "Second pass", nick = "second")]
    Second = 1,
}

glib::wrapper! {
    pub struct VVenC(ObjectSubclass<imp::VVenC>) @extends gst_video::VideoEncoder, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "vvenc",
        gst::Rank::PRIMARY,
        VVenC::static_type(),
    )?;
    SpeedPreset::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    Profile::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    Tier::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    Level::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    DecodingRefreshType::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    HdrMode::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    Ok(())
}
