// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct OcvrCtrl(ObjectSubclass<imp::OcvrCtrl>) @extends gst::Element, gst::Object;
}

unsafe impl Send for OcvrCtrl {}
unsafe impl Sync for OcvrCtrl {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ocvrctrl",
        gst::Rank::None,
        OcvrCtrl::static_type(),
    )
}
