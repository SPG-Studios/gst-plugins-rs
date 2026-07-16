// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segbackgroundimagegl`: the GL twin of `segbackgroundimage`. Composites the
//! sharp object (via `segmaskalphagl`) over a still image on the GPU:
//!
//! ```text
//! sink ! segmaskalphagl ──────────────────────────────────────────────┐ (fg, zorder 1, alpha)
//! filesrc ! decodebin ! imagefreeze ! videoscale ! capsfilter ! glupload ┤ glvideomixer ! src
//!                                                                      ┘ (bg, zorder 0)
//! ```
//!
//! Input and output are `video/x-raw(memory:GLMemory)` (put a `glupload`
//! upstream and a `gldownload` downstream as needed). The image is decoded and
//! scaled on the CPU, then uploaded; a caps probe on the sink sizes it to the
//! frame.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::{LazyLock, OnceLock};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segbackgroundimagegl",
        gst::DebugColorFlags::empty(),
        Some("GL segmentation background image bin"),
    )
});

fn gl_caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .features(["memory:GLMemory"])
        .build()
}

#[derive(Default)]
pub struct SegBackgroundImageGl {
    mask: OnceLock<gst::Element>,
    filesrc: OnceLock<gst::Element>,
    bg_capsfilter: OnceLock<gst::Element>,
    sinkpad: OnceLock<gst::GhostPad>,
    srcpad: OnceLock<gst::GhostPad>,
}

impl SegBackgroundImageGl {
    fn build_topology(&self) -> Result<(), glib::BoolError> {
        let obj = self.obj();
        let bin = obj.upcast_ref::<gst::Bin>();
        let make = |factory: &str| gst::ElementFactory::make(factory).build();

        // Foreground: GL frame with the mask written into its alpha.
        let mask = make("segmaskalphagl")?;

        // Background: decoded still image, scaled on the CPU then uploaded to GL.
        let filesrc = make("filesrc")?;
        let decodebin = make("decodebin")?;
        let imagefreeze = make("imagefreeze")?;
        let videoscale = make("videoscale")?;
        let bg_capsfilter = make("capsfilter")?;
        let glupload = make("glupload")?;

        let mixer = make("glvideomixer")?;

        bin.add_many([
            &mask,
            &filesrc,
            &decodebin,
            &imagefreeze,
            &videoscale,
            &bg_capsfilter,
            &glupload,
            &mixer,
        ])?;

        filesrc.link(&decodebin)?;
        gst::Element::link_many([&imagefreeze, &videoscale, &bg_capsfilter, &glupload])?;

        let imagefreeze_weak = imagefreeze.downgrade();
        decodebin.connect_pad_added(move |_dbin, src_pad| {
            let Some(imagefreeze) = imagefreeze_weak.upgrade() else {
                return;
            };
            let sink = imagefreeze.static_pad("sink").unwrap();
            if !sink.is_linked() {
                let _ = src_pad.link(&sink);
            }
        });

        let bg_pad = mixer
            .request_pad_simple("sink_%u")
            .ok_or_else(|| glib::bool_error!("glvideomixer has no request sink pad"))?;
        bg_pad.set_property("zorder", 0u32);
        glupload
            .static_pad("src")
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

        let sinkpad = gst::GhostPad::from_template(&obj.pad_template("sink").unwrap());
        sinkpad.set_target(Some(&mask.static_pad("sink").unwrap()))?;
        let srcpad = gst::GhostPad::from_template(&obj.pad_template("src").unwrap());
        srcpad.set_target(Some(&mixer.static_pad("src").unwrap()))?;

        // Size the background image to the frame (its width/height come through
        // even on GL memory caps).
        let capsfilter_weak = bg_capsfilter.downgrade();
        sinkpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::EventView::Caps(caps_event)) = info.event().map(|e| e.view())
                && let Some(capsfilter) = capsfilter_weak.upgrade()
                && let Ok(vinfo) = gst_video::VideoInfo::from_caps(&caps_event.caps_owned())
            {
                let bg_caps = gst::Caps::builder("video/x-raw")
                    .field("width", vinfo.width() as i32)
                    .field("height", vinfo.height() as i32)
                    .build();
                capsfilter.set_property("caps", &bg_caps);
            }
            gst::PadProbeReturn::Ok
        });

        let _ = self.mask.set(mask);
        let _ = self.filesrc.set(filesrc);
        let _ = self.bg_capsfilter.set(bg_capsfilter);
        let _ = self.sinkpad.set(sinkpad);
        let _ = self.srcpad.set(srcpad);
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SegBackgroundImageGl {
    const NAME: &'static str = "GstRsSegBackgroundImageGl";
    type Type = super::SegBackgroundImageGl;
    type ParentType = gst::Bin;
}

impl ObjectImpl for SegBackgroundImageGl {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
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
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "location" => {
                if let Some(filesrc) = self.filesrc.get() {
                    filesrc.set_property_from_value("location", value);
                }
            }
            "selected-types" | "feather" | "invert" => {
                if let Some(mask) = self.mask.get() {
                    mask.set_property_from_value(pspec.name(), value);
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => self
                .filesrc
                .get()
                .map(|f| f.property_value("location"))
                .unwrap_or_else(|| None::<String>.to_value()),
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
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        if let Err(err) = self.build_topology() {
            gst::error!(CAT, imp = self, "failed to build bin topology: {err}");
            return;
        }
        let obj = self.obj();
        obj.add_pad(self.sinkpad.get().unwrap()).unwrap();
        obj.add_pad(self.srcpad.get().unwrap()).unwrap();
    }
}

impl GstObjectImpl for SegBackgroundImageGl {}

impl ElementImpl for SegBackgroundImageGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Segmentation Background Image",
                "Filter/Effect/Video",
                "Replaces the background outside an analytics segmentation mask with a still \
                 image on the GPU, keeping the detected object sharp",
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

impl BinImpl for SegBackgroundImageGl {}
