// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segbackgroundblur`: a convenience bin that blurs (or, with the object
//! kept sharp, "portrait-modes") the background outside an analytics
//! segmentation mask.
//!
//! Internally it splits the stream, blurs one copy, turns the segmentation mask
//! into the alpha of the other (sharp) copy via [`crate::segmaskalpha`], and
//! composites the sharp foreground over the blurred background:
//!
//! ```text
//!             ┌ queue ! videoconvert ! gaussianblur ! videoconvert ─┐ (bg, zorder 0)
//! sink ! tee ─┤                                                      ├ compositor ! videoconvert ! src
//!             └ queue ! videoconvert ! segmaskalpha ────────────────┘ (fg, zorder 1, alpha)
//! ```
//!
//! Every element here is stock except `segmaskalpha`; the analytics meta
//! survives the internal `videoconvert`/`gaussianblur` copies (its meta carries a
//! copy transform). To replace the background with an image instead of blurring,
//! use the same topology with an image source on the background pad.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::{LazyLock, OnceLock};

/// Default gaussian blur strength applied to the background.
const DEFAULT_SIGMA: f64 = 6.0;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segbackgroundblur",
        gst::DebugColorFlags::empty(),
        Some("Segmentation background blur bin"),
    )
});

#[derive(Default)]
pub struct SegBackgroundBlur {
    // The segmaskalpha and gaussianblur children, kept for property forwarding.
    // Built in `build_topology`; absent if a required factory was unavailable.
    mask: OnceLock<gst::Element>,
    blur: OnceLock<gst::Element>,
    sinkpad: OnceLock<gst::GhostPad>,
    srcpad: OnceLock<gst::GhostPad>,
}

impl SegBackgroundBlur {
    fn build_topology(&self) -> Result<(), glib::BoolError> {
        let obj = self.obj();
        let bin = obj.upcast_ref::<gst::Bin>();

        let make = |factory: &str| gst::ElementFactory::make(factory).build();

        // segmaskalpha and gaussianblur may be unavailable (optional plugins); a
        // failure here surfaces as a build error rather than a panic/abort.
        let mask = make("segmaskalpha")?;
        let blur = make("gaussianblur")?;
        let tee = make("tee")?;
        let q_fg = make("queue")?;
        let conv_fg = make("videoconvert")?;
        let q_bg = make("queue")?;
        let conv_bg_in = make("videoconvert")?;
        let conv_bg_out = make("videoconvert")?;
        let compositor = make("compositor")?;
        let conv_out = make("videoconvert")?;

        blur.set_property("sigma", DEFAULT_SIGMA);

        bin.add_many([
            &tee,
            &q_fg,
            &conv_fg,
            &mask,
            &q_bg,
            &conv_bg_in,
            &blur,
            &conv_bg_out,
            &compositor,
            &conv_out,
        ])?;

        // Foreground: sharp frame, mask written into alpha.
        gst::Element::link_many([&tee, &q_fg, &conv_fg, &mask])?;
        // Background: blurred copy.
        gst::Element::link_many([&tee, &q_bg, &conv_bg_in, &blur, &conv_bg_out])?;

        // Compositor: background underneath (zorder 0), foreground on top
        // (zorder 1) so its per-pixel alpha reveals the blur behind the object.
        let bg_pad = compositor
            .request_pad_simple("sink_%u")
            .ok_or_else(|| glib::bool_error!("compositor has no request sink pad"))?;
        bg_pad.set_property("zorder", 0u32);
        conv_bg_out
            .static_pad("src")
            .unwrap()
            .link(&bg_pad)
            .map_err(|e| glib::bool_error!("linking background to compositor failed: {e}"))?;

        let fg_pad = compositor
            .request_pad_simple("sink_%u")
            .ok_or_else(|| glib::bool_error!("compositor has no request sink pad"))?;
        fg_pad.set_property("zorder", 1u32);
        mask.static_pad("src")
            .unwrap()
            .link(&fg_pad)
            .map_err(|e| glib::bool_error!("linking foreground to compositor failed: {e}"))?;

        compositor.link(&conv_out)?;

        self.sinkpad
            .get()
            .unwrap()
            .set_target(Some(&tee.static_pad("sink").unwrap()))?;
        self.srcpad
            .get()
            .unwrap()
            .set_target(Some(&conv_out.static_pad("src").unwrap()))?;

        let _ = self.mask.set(mask);
        let _ = self.blur.set(blur);
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SegBackgroundBlur {
    const NAME: &'static str = "GstRsSegBackgroundBlur";
    type Type = super::SegBackgroundBlur;
    type ParentType = gst::Bin;
}

impl ObjectImpl for SegBackgroundBlur {
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
                glib::ParamSpecDouble::builder("sigma")
                    .nick("Sigma")
                    .blurb("Background gaussian blur strength")
                    .minimum(-20.0)
                    .maximum(20.0)
                    .default_value(DEFAULT_SIGMA)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "selected-types" | "feather" | "invert" => {
                if let Some(mask) = self.mask.get() {
                    mask.set_property_from_value(pspec.name(), value);
                }
            }
            "sigma" => {
                if let Some(blur) = self.blur.get() {
                    blur.set_property_from_value("sigma", value);
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "selected-types" => self
                .mask
                .get()
                .map(|m| m.property_value("selected-types"))
                .unwrap_or_else(|| None::<String>.to_value()),
            "feather" => self
                .mask
                .get()
                .map(|m| m.property_value("feather"))
                .unwrap_or_else(|| 0u32.to_value()),
            "invert" => self
                .mask
                .get()
                .map(|m| m.property_value("invert"))
                .unwrap_or_else(|| false.to_value()),
            "sigma" => self
                .blur
                .get()
                .map(|b| b.property_value("sigma"))
                .unwrap_or_else(|| DEFAULT_SIGMA.to_value()),
            _ => unimplemented!(),
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

impl GstObjectImpl for SegBackgroundBlur {}

impl ElementImpl for SegBackgroundBlur {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Background Blur",
                "Filter/Effect/Video",
                "Blurs the background outside an analytics segmentation mask, keeping the \
                 detected object sharp (portrait mode)",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new().build();
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

impl BinImpl for SegBackgroundBlur {}
