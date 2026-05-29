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

use gst_base::prelude::BaseTransformExt;
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use crate::render::{AnalyticsFrame, DrawCommand, RenderContext};

use std::sync::{LazyLock, Mutex};

const DEFAULT_RENDER_ENABLED: bool = false;
const DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_DRAW_LABELS: bool = true;
const DEFAULT_DRAW_TRACKING_LABELS: bool = true;
const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_FILLED_BOX: bool = false;
const DEFAULT_EXPIRE_OVERLAY: u64 = 1_000_000_000;
const DEFAULT_TRACKING_OUTLINE_COLORS: bool = true;

#[derive(Debug, Clone, Copy)]
struct Settings {
    render_enabled: bool,
    object_detection_outline_color: u32,
    draw_labels: bool,
    draw_tracking_labels: bool,
    labels_color: u32,
    filled_box: bool,
    expire_overlay: u64,
    tracking_outline_colors: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_enabled: DEFAULT_RENDER_ENABLED,
            object_detection_outline_color: DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR,
            draw_labels: DEFAULT_DRAW_LABELS,
            draw_tracking_labels: DEFAULT_DRAW_TRACKING_LABELS,
            labels_color: DEFAULT_LABELS_COLOR,
            filled_box: DEFAULT_FILLED_BOX,
            expire_overlay: DEFAULT_EXPIRE_OVERLAY,
            tracking_outline_colors: DEFAULT_TRACKING_OUTLINE_COLORS,
        }
    }
}

#[derive(Default)]
pub struct ObjectDetectionOverlay {
    render_context: Mutex<RenderContext>,
    settings: Mutex<Settings>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "odoverlay",
        gst::DebugColorFlags::empty(),
        Some("Object detection overlay skeleton"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for ObjectDetectionOverlay {
    const NAME: &'static str = "GstObjectDetectionOverlay";
    type Type = super::ObjectDetectionOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for ObjectDetectionOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("object-detection-outline-color")
                    .nick("Object detection outline color")
                    .blurb("Outline color for object detection boxes")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_OBJECT_DETECTION_OUTLINE_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-labels")
                    .nick("Draw labels")
                    .blurb("Draw class and confidence labels")
                    .default_value(DEFAULT_DRAW_LABELS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-tracking-labels")
                    .nick("Draw tracking labels")
                    .blurb("Draw tracking labels when tracking metadata exists")
                    .default_value(DEFAULT_DRAW_TRACKING_LABELS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("labels-color")
                    .nick("Labels color")
                    .blurb("Text labels color")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_LABELS_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("filled-box")
                    .nick("Filled box")
                    .blurb("Fill object detection boxes instead of drawing outlines only")
                    .default_value(DEFAULT_FILLED_BOX)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt64::builder("expire-overlay")
                    .nick("Expire overlay")
                    .blurb("Duration in nanoseconds to keep last overlay when metadata is missing")
                    .minimum(0)
                    .maximum(u64::MAX)
                    .default_value(DEFAULT_EXPIRE_OVERLAY)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("tracking-outline-colors")
                    .nick("Tracking outline colors")
                    .blurb("Use tracking-based dynamic outline colors")
                    .default_value(DEFAULT_TRACKING_OUTLINE_COLORS)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "render-enabled" => {
                let render_enabled = value.get().expect("type checked upstream");
                let mut settings = self.settings.lock().unwrap();
                settings.render_enabled = render_enabled;

                let passthrough = !render_enabled;
                self.obj()
                    .upcast_ref::<gst_base::BaseTransform>()
                    .set_passthrough(passthrough);

                gst::info!(
                    CAT,
                    imp = self,
                    "render-enabled set to {}, passthrough={}",
                    render_enabled,
                    passthrough
                );
            }
            "object-detection-outline-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.object_detection_outline_color =
                    value.get().expect("type checked upstream");
            }
            "draw-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_labels = value.get().expect("type checked upstream");
            }
            "draw-tracking-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_tracking_labels = value.get().expect("type checked upstream");
            }
            "labels-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.labels_color = value.get().expect("type checked upstream");
            }
            "filled-box" => {
                let mut settings = self.settings.lock().unwrap();
                settings.filled_box = value.get().expect("type checked upstream");
            }
            "expire-overlay" => {
                let mut settings = self.settings.lock().unwrap();
                settings.expire_overlay = value.get().expect("type checked upstream");
            }
            "tracking-outline-colors" => {
                let mut settings = self.settings.lock().unwrap();
                settings.tracking_outline_colors = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "render-enabled" => {
                let settings = self.settings.lock().unwrap();
                settings.render_enabled.to_value()
            }
            "object-detection-outline-color" => {
                let settings = self.settings.lock().unwrap();
                settings.object_detection_outline_color.to_value()
            }
            "draw-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_labels.to_value()
            }
            "draw-tracking-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_tracking_labels.to_value()
            }
            "labels-color" => {
                let settings = self.settings.lock().unwrap();
                settings.labels_color.to_value()
            }
            "filled-box" => {
                let settings = self.settings.lock().unwrap();
                settings.filled_box.to_value()
            }
            "expire-overlay" => {
                let settings = self.settings.lock().unwrap();
                settings.expire_overlay.to_value()
            }
            "tracking-outline-colors" => {
                let settings = self.settings.lock().unwrap();
                settings.tracking_outline_colors.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        self.obj()
            .upcast_ref::<gst_base::BaseTransform>()
            .set_passthrough(!DEFAULT_RENDER_ENABLED);
    }
}

impl GstObjectImpl for ObjectDetectionOverlay {}

impl ElementImpl for ObjectDetectionOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Object Detection Overlay (skeleton)",
                "Filter/Editor/Video",
                "Object detection overlay skeleton with passthrough toggle",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("video/x-raw").build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for ObjectDetectionOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl VideoFilterImpl for ObjectDetectionOverlay {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut render_context = self.render_context.lock().unwrap();
        render_context.render(frame, &AnalyticsFrame::default(), &[DrawCommand::NoOp])?;

        Ok(gst::FlowSuccess::Ok)
    }
}
