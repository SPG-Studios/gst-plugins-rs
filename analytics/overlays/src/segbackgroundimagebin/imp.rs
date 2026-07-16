// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segbackgroundimagebin`: auto-selecting CPU/GL segmentation background image.
//!
//! Wraps `segbackgroundimage` (CPU) and `segbackgroundimagegl` (GL) and picks one
//! from the input memory type, on the shared [`crate::autobin`] machinery. Both
//! children share the same property set, so all four properties forward cleanly.
//! Properties are declared here (not introspected) so class-init never
//! instantiates the CPU child — see the note in `segbackgroundblurbin`.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::LazyLock;

use crate::autobin::{self, AutoOverlayBin};

const CPU_FACTORY: &str = "segbackgroundimage";
const GL_FACTORY: &str = "segbackgroundimagegl";

static PROPERTIES: LazyLock<(Vec<glib::ParamSpec>, gst::Structure)> = LazyLock::new(|| {
    let specs = vec![
        glib::ParamSpecString::builder("location")
            .nick("Location")
            .blurb("Path to the background image file")
            .mutable_ready()
            .build(),
        glib::ParamSpecString::builder("selected-types")
            .nick("Selected types")
            .blurb("Foreground classes (semicolon-separated); empty means any object")
            .mutable_playing()
            .build(),
        glib::ParamSpecUInt::builder("feather")
            .nick("Feather")
            .blurb("Radius in pixels of the alpha-edge feather")
            .minimum(0)
            .maximum(128)
            .default_value(0)
            .mutable_playing()
            .build(),
        glib::ParamSpecBoolean::builder("invert")
            .nick("Invert")
            .blurb("Replace the object instead of the background")
            .default_value(false)
            .mutable_playing()
            .build(),
    ];

    let mut defaults = gst::Structure::new_empty("segbackgroundimagebin-defaults");
    defaults.set_value("location", None::<String>.to_send_value());
    defaults.set_value("selected-types", None::<String>.to_send_value());
    defaults.set_value("feather", 0u32.to_send_value());
    defaults.set_value("invert", false.to_send_value());

    (specs, defaults)
});

pub struct SegBackgroundImageBin {
    inner: AutoOverlayBin,
}

#[glib::object_subclass]
impl ObjectSubclass for SegBackgroundImageBin {
    const NAME: &'static str = "GstRsSegBackgroundImageBin";
    type Type = super::SegBackgroundImageBin;
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

impl ObjectImpl for SegBackgroundImageBin {
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

impl GstObjectImpl for SegBackgroundImageBin {}

impl ElementImpl for SegBackgroundImageBin {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Background Image (auto CPU/GL)",
                "Filter/Effect/Video",
                "Wraps segbackgroundimage/segbackgroundimagegl and selects one from the input \
                 memory type",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        autobin::pad_templates()
    }
}

impl BinImpl for SegBackgroundImageBin {}
