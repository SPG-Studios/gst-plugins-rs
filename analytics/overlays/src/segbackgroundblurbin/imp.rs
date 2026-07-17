// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segbackgroundblurbin`: auto-selecting CPU/GL segmentation background blur.
//!
//! Wraps `segbackgroundblur` (CPU) and `segbackgroundblurgl` (GL) and picks one
//! from the input memory type, so `... ! segbackgroundblurbin ! ...` just works
//! whether frames arrive in system memory or on the GPU. Built on the shared
//! [`crate::autobin`] machinery (see the overlay auto-bins). The `sigma` property
//! only applies to the CPU child (the GL blur is a fixed kernel); it is accepted
//! and cached but ignored while the GL child is active.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::LazyLock;

use crate::autobin::{self, AutoOverlayBin};

const CPU_FACTORY: &str = "segbackgroundblur";
const GL_FACTORY: &str = "segbackgroundblurgl";
const DEFAULT_SIGMA: f64 = 6.0;

// Properties are declared here rather than introspected from the CPU child: the
// CPU child is a bin that instantiates `gaussianblur`/`compositor` in its
// constructor, and building it just to read its properties would fail (and,
// historically, panic) during plugin scanning when those plugins are not yet
// loaded. The set mirrors `segbackgroundblur`'s (the GL child lacks `sigma`,
// which the auto-bin caches but does not forward while GL is active).
static PROPERTIES: LazyLock<(Vec<glib::ParamSpec>, gst::Structure)> = LazyLock::new(|| {
    let specs = vec![
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
            .blurb("Blur the object instead of the background")
            .default_value(false)
            .mutable_playing()
            .build(),
        glib::ParamSpecDouble::builder("sigma")
            .nick("Sigma")
            .blurb("Background gaussian blur strength (CPU child only)")
            .minimum(-20.0)
            .maximum(20.0)
            .default_value(DEFAULT_SIGMA)
            .mutable_playing()
            .build(),
    ];

    let mut defaults = gst::Structure::new_empty("segbackgroundblurbin-defaults");
    defaults.set_value("selected-types", None::<String>.to_send_value());
    defaults.set_value("feather", 0u32.to_send_value());
    defaults.set_value("invert", false.to_send_value());
    defaults.set_value("sigma", DEFAULT_SIGMA.to_send_value());

    (specs, defaults)
});

pub struct SegBackgroundBlurBin {
    inner: AutoOverlayBin,
}

#[glib::object_subclass]
impl ObjectSubclass for SegBackgroundBlurBin {
    const NAME: &'static str = "GstRsSegBackgroundBlurBin";
    type Type = super::SegBackgroundBlurBin;
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

impl ObjectImpl for SegBackgroundBlurBin {
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

impl GstObjectImpl for SegBackgroundBlurBin {}

impl ElementImpl for SegBackgroundBlurBin {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Background Blur (auto CPU/GL)",
                "Filter/Effect/Video",
                "Wraps segbackgroundblur/segbackgroundblurgl and selects one from the input \
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

impl BinImpl for SegBackgroundBlurBin {}
