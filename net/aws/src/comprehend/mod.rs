// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
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
    pub struct Comprehend(ObjectSubclass<imp::Comprehend>) @extends gst::Element, gst::Object;
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstAwsComprehendDetectionType")]
#[non_exhaustive]
pub enum AwsComprehendDetectionType {
    #[enum_value(name = "DetectEntities", nick = "detect-entities")]
    DetectEntities = 0,
    #[enum_value(name = "DetectKeyPhrases", nick = "detect-key-phrases")]
    DetectKeyPhrases = 1,
    #[enum_value(name = "DetectPiiEntities", nick = "detect-pii-entities")]
    DetectPiiEntities = 2,
    #[enum_value(name = "DetectSentiment", nick = "detect-sentiment")]
    DetectSentiment = 3,
    #[enum_value(name = "DetectSyntax", nick = "detect-syntax")]
    DetectSyntax = 4,
    #[enum_value(name = "DetectTargetedSentiment", nick = "detect-targeted-sentiment")]
    DetectTargetedSentiment = 5,
    #[enum_value(name = "DetectToxicContent", nick = "detect-toxic-content")]
    DetectToxicContent = 6,
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    AwsComprehendDetectionType::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "awscomprehend",
        gst::Rank::NONE,
        Comprehend::static_type(),
    )
}
