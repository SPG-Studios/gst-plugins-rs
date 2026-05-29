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

use std::sync::{LazyLock, Mutex};

const DEFAULT_RENDER_ENABLED: bool = false;
const DEFAULT_KEYPOINT_COLOR: u32 = 0xFFFF_0000;
const DEFAULT_KEYPOINT_RADIUS: f64 = 3.0;
const DEFAULT_DRAW_LABELS: bool = true;
const DEFAULT_LABELS_COLOR: u32 = 0xFFFF_FFFF;
const DEFAULT_DRAW_SKELETON: bool = false;
const DEFAULT_SKELETON_COLOR: u32 = 0xFF00_FF00;
const DEFAULT_SKELETON_LINE_WIDTH: f64 = 2.0;

#[derive(Debug, Clone)]
struct Settings {
    render_enabled: bool,
    keypoint_color: u32,
    keypoint_radius: f64,
    draw_labels: bool,
    labels_color: u32,
    draw_skeleton: bool,
    skeleton_color: u32,
    skeleton_line_width: f64,
    semantic_tag: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_enabled: DEFAULT_RENDER_ENABLED,
            keypoint_color: DEFAULT_KEYPOINT_COLOR,
            keypoint_radius: DEFAULT_KEYPOINT_RADIUS,
            draw_labels: DEFAULT_DRAW_LABELS,
            labels_color: DEFAULT_LABELS_COLOR,
            draw_skeleton: DEFAULT_DRAW_SKELETON,
            skeleton_color: DEFAULT_SKELETON_COLOR,
            skeleton_line_width: DEFAULT_SKELETON_LINE_WIDTH,
            semantic_tag: None,
        }
    }
}

#[derive(Default)]
pub struct KeypointsOverlay {
    settings: Mutex<Settings>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "keypointsoverlay",
        gst::DebugColorFlags::empty(),
        Some("Keypoints overlay skeleton"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for KeypointsOverlay {
    const NAME: &'static str = "GstKeypointsOverlay";
    type Type = super::KeypointsOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for KeypointsOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("keypoint-color")
                    .nick("Keypoint color")
                    .blurb("Color used to draw keypoints")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_KEYPOINT_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("keypoint-radius")
                    .nick("Keypoint radius")
                    .blurb("Radius in pixels used for keypoints")
                    .minimum(1.0)
                    .maximum(20.0)
                    .default_value(DEFAULT_KEYPOINT_RADIUS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-labels")
                    .nick("Draw labels")
                    .blurb("Draw keypoint confidence labels")
                    .default_value(DEFAULT_DRAW_LABELS)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("labels-color")
                    .nick("Labels color")
                    .blurb("Color used for keypoint labels")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_LABELS_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-skeleton")
                    .nick("Draw skeleton")
                    .blurb("Draw skeleton relations between keypoints")
                    .default_value(DEFAULT_DRAW_SKELETON)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("skeleton-color")
                    .nick("Skeleton color")
                    .blurb("Color used for skeleton lines")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_SKELETON_COLOR)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("skeleton-line-width")
                    .nick("Skeleton line width")
                    .blurb("Line width in pixels for skeleton rendering")
                    .minimum(1.0)
                    .maximum(10.0)
                    .default_value(DEFAULT_SKELETON_LINE_WIDTH)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("semantic-tag")
                    .nick("Semantic tag")
                    .blurb("Semantic tag prefix used to filter grouped keypoints")
                    .default_value(None)
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
            "keypoint-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.keypoint_color = value.get().expect("type checked upstream");
            }
            "keypoint-radius" => {
                let mut settings = self.settings.lock().unwrap();
                settings.keypoint_radius = value.get().expect("type checked upstream");
            }
            "draw-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_labels = value.get().expect("type checked upstream");
            }
            "labels-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.labels_color = value.get().expect("type checked upstream");
            }
            "draw-skeleton" => {
                let mut settings = self.settings.lock().unwrap();
                settings.draw_skeleton = value.get().expect("type checked upstream");
            }
            "skeleton-color" => {
                let mut settings = self.settings.lock().unwrap();
                settings.skeleton_color = value.get().expect("type checked upstream");
            }
            "skeleton-line-width" => {
                let mut settings = self.settings.lock().unwrap();
                settings.skeleton_line_width = value.get().expect("type checked upstream");
            }
            "semantic-tag" => {
                let mut settings = self.settings.lock().unwrap();
                settings.semantic_tag = value.get().expect("type checked upstream");
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
            "keypoint-color" => {
                let settings = self.settings.lock().unwrap();
                settings.keypoint_color.to_value()
            }
            "keypoint-radius" => {
                let settings = self.settings.lock().unwrap();
                settings.keypoint_radius.to_value()
            }
            "draw-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_labels.to_value()
            }
            "labels-color" => {
                let settings = self.settings.lock().unwrap();
                settings.labels_color.to_value()
            }
            "draw-skeleton" => {
                let settings = self.settings.lock().unwrap();
                settings.draw_skeleton.to_value()
            }
            "skeleton-color" => {
                let settings = self.settings.lock().unwrap();
                settings.skeleton_color.to_value()
            }
            "skeleton-line-width" => {
                let settings = self.settings.lock().unwrap();
                settings.skeleton_line_width.to_value()
            }
            "semantic-tag" => {
                let settings = self.settings.lock().unwrap();
                settings.semantic_tag.to_value()
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

impl GstObjectImpl for KeypointsOverlay {}

impl ElementImpl for KeypointsOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Keypoints Overlay (skeleton)",
                "Filter/Editor/Video",
                "Keypoints overlay skeleton with passthrough toggle",
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

impl BaseTransformImpl for KeypointsOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl VideoFilterImpl for KeypointsOverlay {
    fn transform_frame_ip(
        &self,
        _frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        Ok(gst::FlowSuccess::Ok)
    }
}
