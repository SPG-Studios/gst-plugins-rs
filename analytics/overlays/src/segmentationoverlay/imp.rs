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
const DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE: u32 = 10;

#[derive(Debug, Clone)]
struct Settings {
    render_enabled: bool,
    hint_maximum_segment_type: u32,
    selected_types: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            render_enabled: DEFAULT_RENDER_ENABLED,
            hint_maximum_segment_type: DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE,
            selected_types: None,
        }
    }
}

#[derive(Default)]
pub struct SegmentationOverlay {
    render_context: Mutex<RenderContext>,
    settings: Mutex<Settings>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segoverlay",
        gst::DebugColorFlags::empty(),
        Some("Segmentation overlay skeleton"),
    )
});

#[glib::object_subclass]
impl ObjectSubclass for SegmentationOverlay {
    const NAME: &'static str = "GstSegmentationOverlay";
    type Type = super::SegmentationOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for SegmentationOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("hint-maximum-segment-type")
                    .nick("Hint maximum segment type")
                    .blurb("Hint for expected maximum segment type value")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("selected-types")
                    .nick("Selected types")
                    .blurb("Semicolon-separated type names to render")
                    .default_value(None)
                    .mutable_ready()
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
            "hint-maximum-segment-type" => {
                let mut settings = self.settings.lock().unwrap();
                settings.hint_maximum_segment_type = value.get().expect("type checked upstream");
            }
            "selected-types" => {
                let mut settings = self.settings.lock().unwrap();
                settings.selected_types = value.get().expect("type checked upstream");
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
            "hint-maximum-segment-type" => {
                let settings = self.settings.lock().unwrap();
                settings.hint_maximum_segment_type.to_value()
            }
            "selected-types" => {
                let settings = self.settings.lock().unwrap();
                settings.selected_types.to_value()
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

impl GstObjectImpl for SegmentationOverlay {}

impl ElementImpl for SegmentationOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Overlay (skeleton)",
                "Filter/Editor/Video",
                "Segmentation overlay skeleton with passthrough toggle",
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

impl BaseTransformImpl for SegmentationOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
}

impl VideoFilterImpl for SegmentationOverlay {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut render_context = self.render_context.lock().unwrap();
        render_context.render(frame, &AnalyticsFrame::default(), &[DrawCommand::NoOp])?;

        Ok(gst::FlowSuccess::Ok)
    }
}
