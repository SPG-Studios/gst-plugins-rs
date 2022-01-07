// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OcvrHint(ObjectSubclass<imp::OcvrHint>) @extends gst::Element, gst::Object;
}

unsafe impl Send for OcvrHint {}
unsafe impl Sync for OcvrHint {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ocvrhint",
        gst::Rank::None,
        OcvrHint::static_type(),
    )
}
