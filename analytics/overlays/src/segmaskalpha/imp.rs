// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! `segmaskalpha`: turn an analytics segmentation mask into the video frame's
//! alpha channel.
//!
//! The segmentation overlay *draws* colours on top of the frame; this element
//! instead writes the mask into the frame's **alpha** — opaque where a selected
//! object is, transparent elsewhere (or the reverse with `invert`). That lets a
//! downstream `compositor`/`glvideomixer` blend the frame over a blurred copy or
//! a replacement image to produce a "blur/replace the background" effect. See the
//! `segbackgroundblur` bin for the full pipeline.
//!
//! The low-resolution mask (e.g. 160×160 from yolov8-seg) is turned into a binary
//! object map at its native resolution, then **bilinearly** upscaled into the
//! frame so edges feather instead of blocking up; an optional `feather` box-blur
//! softens them further. The mask access and class (`selected-types`) filtering
//! are shared with the segmentation overlay (`super::segmentationoverlay::masks`).

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gst_video::prelude::VideoFrameExt;
use gst_video::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use super::alpha::build_frame_alpha;
use crate::segmentationoverlay::masks::State;

const DEFAULT_INVERT: bool = false;
const DEFAULT_FEATHER: u32 = 0;
const MAX_FEATHER: u32 = 128;

#[derive(Debug, Clone)]
struct Settings {
    selected_types: Option<String>,
    invert: bool,
    feather: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            selected_types: None,
            invert: DEFAULT_INVERT,
            feather: DEFAULT_FEATHER,
        }
    }
}

#[derive(Default)]
pub struct SegMaskAlpha {
    settings: Mutex<Settings>,
    // Reused from the segmentation overlay purely for its `selected-types`
    // class-filter cache; the colour fields go unused here.
    state: Mutex<State>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segmaskalpha",
        gst::DebugColorFlags::empty(),
        Some("Segmentation mask to alpha"),
    )
});

// Only packed formats with an alpha channel: the element writes one alpha byte
// per pixel and leaves the colour bytes untouched.
static ALPHA_CAPS: LazyLock<gst::Caps> = LazyLock::new(|| {
    gst_video::VideoCapsBuilder::new()
        .format_list([
            gst_video::VideoFormat::Rgba,
            gst_video::VideoFormat::Bgra,
            gst_video::VideoFormat::Argb,
            gst_video::VideoFormat::Abgr,
            gst_video::VideoFormat::Ayuv,
        ])
        .build()
});

/// Byte offset of the alpha component within each 4-byte pixel, per format.
fn alpha_offset(format: gst_video::VideoFormat) -> Option<usize> {
    match format {
        gst_video::VideoFormat::Rgba | gst_video::VideoFormat::Bgra => Some(3),
        gst_video::VideoFormat::Argb
        | gst_video::VideoFormat::Abgr
        | gst_video::VideoFormat::Ayuv => Some(0),
        _ => None,
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SegMaskAlpha {
    const NAME: &'static str = "GstRsSegMaskAlpha";
    type Type = super::SegMaskAlpha;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for SegMaskAlpha {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("selected-types")
                    .nick("Selected types")
                    .blurb(
                        "Semicolon-separated class names to treat as foreground; \
                         empty means every detected (non-zero) mask value",
                    )
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("invert")
                    .nick("Invert")
                    .blurb(
                        "When false the object is opaque (alpha 255) and the background \
                         transparent; when true the object is transparent",
                    )
                    .default_value(DEFAULT_INVERT)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("feather")
                    .nick("Feather")
                    .blurb("Radius in pixels of an extra box blur applied to the alpha edge")
                    .minimum(0)
                    .maximum(MAX_FEATHER)
                    .default_value(DEFAULT_FEATHER)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "selected-types" => settings.selected_types = value.get().expect("type checked"),
            "invert" => settings.invert = value.get().expect("type checked"),
            "feather" => settings.feather = value.get().expect("type checked"),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "selected-types" => settings.selected_types.to_value(),
            "invert" => settings.invert.to_value(),
            "feather" => settings.feather.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for SegMaskAlpha {}

impl ElementImpl for SegMaskAlpha {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Mask to Alpha",
                "Filter/Effect/Video",
                "Writes an analytics segmentation mask into the frame's alpha channel \
                 (foreground opaque, background transparent) for background blur/replace",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = ALPHA_CAPS.clone();
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

impl BaseTransformImpl for SegMaskAlpha {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl VideoFilterImpl for SegMaskAlpha {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();

        let format = frame.format();
        let Some(offset) = alpha_offset(format) else {
            // Negotiation restricts to alpha formats, so this should not happen.
            gst::error!(CAT, imp = self, "unsupported format {format:?}");
            return Err(gst::FlowError::NotNegotiated);
        };

        let frame_w = frame.width() as usize;
        let frame_h = frame.height() as usize;

        // Hold the state lock across the write so the borrowed alpha scratch
        // buffer (reused across frames, owned by `state`) stays valid.
        let mut state = self.state.lock().unwrap();
        let alpha = build_frame_alpha(
            &mut state,
            settings.selected_types.as_deref(),
            settings.feather,
            frame.buffer(),
            frame_w,
            frame_h,
        );

        // No analytics meta: leave the frame's alpha as it arrived.
        let Some(alpha) = alpha else {
            return Ok(gst::FlowSuccess::Ok);
        };

        let stride = frame.plane_stride()[0].unsigned_abs() as usize;
        let data = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;
        let invert = settings.invert;

        for y in 0..frame_h {
            let row = &mut data[y * stride..y * stride + frame_w * 4];
            for x in 0..frame_w {
                let a = alpha[y * frame_w + x];
                row[x * 4 + offset] = if invert { 255 - a } else { a };
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
