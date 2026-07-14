// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::LazyLock;

use crate::autobin::{self, AutoOverlayBin};

const CPU_FACTORY: &str = "keypointsoverlay";
const GL_FACTORY: &str = "keypointsoverlaygl";

static PROPERTIES: LazyLock<(Vec<glib::ParamSpec>, gst::Structure)> =
    LazyLock::new(|| autobin::forwarded(CPU_FACTORY));

pub struct KeypointsOverlayBin {
    inner: AutoOverlayBin,
}

#[glib::object_subclass]
impl ObjectSubclass for KeypointsOverlayBin {
    const NAME: &'static str = "GstRsKeypointsOverlayBin";
    type Type = super::KeypointsOverlayBin;
    type ParentType = gst::Bin;

    fn with_class(klass: &Self::Class) -> Self {
        let sinkpad = gst::GhostPad::from_template(&klass.pad_template("sink").unwrap());
        let srcpad = gst::GhostPad::from_template(&klass.pad_template("src").unwrap());
        Self {
            inner: AutoOverlayBin::new(
                sinkpad,
                srcpad,
                CPU_FACTORY,
                GL_FACTORY,
                PROPERTIES.1.clone(),
            ),
        }
    }
}

impl ObjectImpl for KeypointsOverlayBin {
    fn properties() -> &'static [glib::ParamSpec] {
        &PROPERTIES.0
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        self.inner.set_property(pspec.name(), value);
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        self.inner.property(pspec.name())
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.inner.constructed(self.obj().upcast_ref());
    }
}

impl GstObjectImpl for KeypointsOverlayBin {}

impl ElementImpl for KeypointsOverlayBin {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Keypoints Overlay (auto CPU/GL)",
                "Filter/Editor/Video",
                "Wraps keypointsoverlay/keypointsoverlaygl and selects one from the input memory type",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        autobin::pad_templates()
    }
}

impl BinImpl for KeypointsOverlayBin {}
