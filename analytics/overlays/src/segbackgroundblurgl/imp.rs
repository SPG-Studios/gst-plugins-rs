// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segbackgroundblurgl`: the GL twin of `segbackgroundblur`. Same topology, on
//! the GPU end to end — the frame never leaves GL memory:
//!
//! ```text
//!             ┌ queue ! gleffects_blur ──────────┐ (bg, zorder 0)
//! sink ! tee ─┤                                   ├ glvideomixer ! src
//!             └ queue ! segmaskalphagl ──────────┘ (fg, zorder 1, alpha)
//! ```
//!
//! Input and output are `video/x-raw(memory:GLMemory)` (put a `glupload` upstream
//! and a `gldownload` downstream as needed). `gleffects_blur` is a fixed 9×9
//! convolution, so there is no blur-strength knob here (unlike the CPU bin's
//! `sigma`).

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::{LazyLock, OnceLock};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segbackgroundblurgl",
        gst::DebugColorFlags::empty(),
        Some("GL segmentation background blur bin"),
    )
});

fn gl_caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .features(["memory:GLMemory"])
        .build()
}

#[derive(Default)]
pub struct SegBackgroundBlurGl {
    // The segmaskalphagl child, kept for property forwarding. Built in
    // `build_topology`; absent if a required factory was unavailable.
    mask: OnceLock<gst::Element>,
    sinkpad: OnceLock<gst::GhostPad>,
    srcpad: OnceLock<gst::GhostPad>,
}

impl SegBackgroundBlurGl {
    fn build_topology(&self) -> Result<(), glib::BoolError> {
        let obj = self.obj();
        let bin = obj.upcast_ref::<gst::Bin>();
        let make = |factory: &str| gst::ElementFactory::make(factory).build();

        // segmaskalphagl and gleffects_blur may be unavailable (optional plugins);
        // a failure surfaces as a build error rather than a panic/abort.
        let mask = make("segmaskalphagl")?;
        let tee = make("tee")?;
        let q_fg = make("queue")?;
        let q_bg = make("queue")?;
        let blur = make("gleffects_blur")?;
        let mixer = make("glvideomixer")?;

        bin.add_many([&tee, &q_fg, &mask, &q_bg, &blur, &mixer])?;

        gst::Element::link_many([&tee, &q_fg, &mask])?;
        gst::Element::link_many([&tee, &q_bg, &blur])?;

        let bg_pad = mixer
            .request_pad_simple("sink_%u")
            .ok_or_else(|| glib::bool_error!("glvideomixer has no request sink pad"))?;
        bg_pad.set_property("zorder", 0u32);
        blur.static_pad("src")
            .unwrap()
            .link(&bg_pad)
            .map_err(|e| glib::bool_error!("linking background to glvideomixer failed: {e}"))?;

        let fg_pad = mixer
            .request_pad_simple("sink_%u")
            .ok_or_else(|| glib::bool_error!("glvideomixer has no request sink pad"))?;
        fg_pad.set_property("zorder", 1u32);
        mask.static_pad("src")
            .unwrap()
            .link(&fg_pad)
            .map_err(|e| glib::bool_error!("linking foreground to glvideomixer failed: {e}"))?;

        self.sinkpad
            .get()
            .unwrap()
            .set_target(Some(&tee.static_pad("sink").unwrap()))?;
        self.srcpad
            .get()
            .unwrap()
            .set_target(Some(&mixer.static_pad("src").unwrap()))?;

        let _ = self.mask.set(mask);
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SegBackgroundBlurGl {
    const NAME: &'static str = "GstRsSegBackgroundBlurGl";
    type Type = super::SegBackgroundBlurGl;
    type ParentType = gst::Bin;
}

impl ObjectImpl for SegBackgroundBlurGl {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
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
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if let Some(mask) = self.mask.get() {
            mask.set_property_from_value(pspec.name(), value);
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match (self.mask.get(), pspec.name()) {
            (Some(mask), name) => mask.property_value(name),
            (None, "selected-types") => None::<String>.to_value(),
            (None, "feather") => 0u32.to_value(),
            (None, _) => false.to_value(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        let _ = self.sinkpad.set(gst::GhostPad::from_template(
            &obj.pad_template("sink").unwrap(),
        ));
        let _ = self.srcpad.set(gst::GhostPad::from_template(
            &obj.pad_template("src").unwrap(),
        ));
        if let Err(err) = self.build_topology() {
            gst::error!(CAT, imp = self, "failed to build bin topology: {err}");
            return;
        }
        obj.add_pad(self.sinkpad.get().unwrap()).unwrap();
        obj.add_pad(self.srcpad.get().unwrap()).unwrap();
    }
}

impl GstObjectImpl for SegBackgroundBlurGl {}

impl ElementImpl for SegBackgroundBlurGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Segmentation Background Blur",
                "Filter/Effect/Video",
                "Blurs the background outside an analytics segmentation mask on the GPU, \
                 keeping the detected object sharp (portrait mode)",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gl_caps();
            let src = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            let sink = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src, sink]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BinImpl for SegBackgroundBlurGl {}
