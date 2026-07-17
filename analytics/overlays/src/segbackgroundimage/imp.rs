// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segbackgroundimage`: replace the background outside an analytics segmentation
//! mask with a still image, keeping the detected object sharp.
//!
//! Same "object over background" compositing as `segbackgroundblur`, but the
//! background layer is a decoded still image (from `location`) scaled to the
//! frame, rather than a blurred copy of the frame:
//!
//! ```text
//! sink ! videoconvert ! segmaskalpha ───────────────────────────────┐ (fg, zorder 1, alpha)
//! filesrc ! decodebin ! imagefreeze ! videoscale ! capsfilter ! videoconvert ┤ compositor ! videoconvert ! src
//!                                                                    ┘ (bg, zorder 0)
//! ```
//!
//! The image is stretched to the negotiated frame size via a caps probe on the
//! sink pad that learns W×H and fixes the background `capsfilter`.

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use std::sync::{LazyLock, OnceLock};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segbackgroundimage",
        gst::DebugColorFlags::empty(),
        Some("Segmentation background image bin"),
    )
});

#[derive(Default)]
pub struct SegBackgroundImage {
    // Children kept for property forwarding / runtime configuration; built in
    // `build_topology`, absent if a required factory was unavailable.
    mask: OnceLock<gst::Element>,
    filesrc: OnceLock<gst::Element>,
    bg_capsfilter: OnceLock<gst::Element>,
    sinkpad: OnceLock<gst::GhostPad>,
    srcpad: OnceLock<gst::GhostPad>,
}

impl SegBackgroundImage {
    fn build_topology(&self) -> Result<(), glib::BoolError> {
        let obj = self.obj();
        let bin = obj.upcast_ref::<gst::Bin>();
        let make = |factory: &str| gst::ElementFactory::make(factory).build();

        // Foreground: sharp frame, segmentation mask written into its alpha.
        let conv_fg = make("videoconvert")?;
        let mask = make("segmaskalpha")?;

        // Background: decoded still image, scaled to the frame size.
        let filesrc = make("filesrc")?;
        let decodebin = make("decodebin")?;
        let imagefreeze = make("imagefreeze")?;
        let videoscale = make("videoscale")?;
        let bg_capsfilter = make("capsfilter")?;
        let conv_bg = make("videoconvert")?;

        let compositor = make("compositor")?;
        let conv_out = make("videoconvert")?;

        bin.add_many([
            &conv_fg,
            &mask,
            &filesrc,
            &decodebin,
            &imagefreeze,
            &videoscale,
            &bg_capsfilter,
            &conv_bg,
            &compositor,
            &conv_out,
        ])?;

        gst::Element::link_many([&conv_fg, &mask])?;
        filesrc.link(&decodebin)?;
        gst::Element::link_many([&imagefreeze, &videoscale, &bg_capsfilter, &conv_bg])?;

        // decodebin exposes its src pad only once the image type is known.
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

        // Compositor: image underneath (zorder 0), foreground on top (zorder 1)
        // so its per-pixel alpha reveals the image behind the object.
        let bg_pad = compositor
            .request_pad_simple("sink_%u")
            .ok_or_else(|| glib::bool_error!("compositor has no request sink pad"))?;
        bg_pad.set_property("zorder", 0u32);
        conv_bg
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

        let sinkpad = gst::GhostPad::from_template(&obj.pad_template("sink").unwrap());
        sinkpad.set_target(Some(&conv_fg.static_pad("sink").unwrap()))?;
        let srcpad = gst::GhostPad::from_template(&obj.pad_template("src").unwrap());
        srcpad.set_target(Some(&conv_out.static_pad("src").unwrap()))?;

        // Learn the frame size from the sink caps and size the background image to
        // match, so the image fills the frame.
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
impl ObjectSubclass for SegBackgroundImage {
    const NAME: &'static str = "GstRsSegBackgroundImage";
    type Type = super::SegBackgroundImage;
    type ParentType = gst::Bin;
}

impl ObjectImpl for SegBackgroundImage {
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

impl GstObjectImpl for SegBackgroundImage {}

impl ElementImpl for SegBackgroundImage {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Background Image",
                "Filter/Effect/Video",
                "Replaces the background outside an analytics segmentation mask with a still \
                 image, keeping the detected object sharp",
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

impl BinImpl for SegBackgroundImage {}
