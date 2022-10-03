// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OcvrHint(ObjectSubclass<imp::OcvrHint>) @extends gst::Element, gst::Object;
}


pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    imp::ContentRate::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    #[cfg(feature = "doc")]
    imp::CaptureRate::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());

    gst::Element::register(
        Some(plugin),
        "ocvrhint",
        gst::Rank::None,
        OcvrHint::static_type(),
    )
}
