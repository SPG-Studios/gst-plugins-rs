// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// GL OD overlay. Reads AnalyticsRelationMeta and renders the object-detection
// overlay with skia's Ganesh GPU backend onto the pipeline's GL textures,
// reusing the CPU element's command generation (`analytics_to_draw_commands`)
// and the shared `render::replay_commands`, so GL and CPU draw identically.
//
// The buffer (needed for the meta) is captured in `before_transform`; the actual
// GPU draw happens in `filter_texture`, which has the GL textures.

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::subclass::BaseTransformMode;
use gst_base::subclass::base_transform::{InputBuffer, PrepareOutputBufferSuccess};
use gst_base::subclass::prelude::*;
use gst_gl::prelude::*;
use gst_gl::subclass::GLFilterMode;
use gst_gl::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use crate::glsupport::GpuState;
use crate::objectdetectionoverlay::commands as od;
use crate::overlay_intent::{LabelIntent, add_label_intents};
use crate::render::{DrawCommand, replay_commands};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "odoverlaygl",
        gst::DebugColorFlags::empty(),
        Some("GL object detection overlay (skia GPU)"),
    )
});

#[derive(Default)]
pub struct ObjectDetectionOverlayGl {
    gpu: Mutex<Option<GpuState>>,
    /// Negotiated frame size (width, height), from `gl_set_caps`.
    dims: Mutex<(i32, i32)>,
    /// Draw commands for the current buffer, produced in `before_transform`.
    pending: Mutex<Vec<DrawCommand>>,
    /// Deferred label intents for the current buffer (defer mode), emitted onto
    /// the output buffer in `prepare_output_buffer`.
    pending_labels: Mutex<Vec<LabelIntent>>,
    /// Rendering settings (the CPU element's `Settings`, reused), driven by the
    /// element's GObject properties.
    settings: Mutex<od::Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for ObjectDetectionOverlayGl {
    const NAME: &'static str = "GstRsObjectDetectionOverlayGl";
    type Type = super::ObjectDetectionOverlayGl;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for ObjectDetectionOverlayGl {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let d = od::Settings::default();
            vec![
                glib::ParamSpecUInt::builder("object-detection-outline-color")
                    .nick("Object detection outline color")
                    .blurb("Outline color for object detection boxes")
                    .default_value(d.object_detection_outline_color)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-labels")
                    .nick("Draw labels")
                    .blurb("Draw class and confidence labels")
                    .default_value(d.draw_labels)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-tracking-labels")
                    .nick("Draw tracking labels")
                    .blurb("Draw tracking labels when tracking metadata exists")
                    .default_value(d.draw_tracking_labels)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("labels-color")
                    .nick("Labels color")
                    .blurb("Text labels color")
                    .default_value(d.labels_color)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("filled-box")
                    .nick("Filled box")
                    .blurb("Fill object detection boxes instead of drawing outlines only")
                    .default_value(d.filled_box)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("tracking-outline-colors")
                    .nick("Tracking outline colors")
                    .blurb("Use tracking-based dynamic outline colors")
                    .default_value(d.tracking_outline_colors)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("defer-labels")
                    .nick("Defer labels")
                    .blurb(
                        "Emit labels as deferred intents for a downstream overlaycompositorgl \
                         instead of placing them here. Requires a compositor downstream, or the \
                         labels are not drawn.",
                    )
                    .default_value(d.defer_labels)
                    .mutable_playing()
                    .build(),
                crate::coordination::priority_param_spec(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        let e = "type checked upstream";
        match pspec.name() {
            "object-detection-outline-color" => {
                settings.object_detection_outline_color = value.get().expect(e)
            }
            "draw-labels" => settings.draw_labels = value.get().expect(e),
            "draw-tracking-labels" => settings.draw_tracking_labels = value.get().expect(e),
            "labels-color" => settings.labels_color = value.get().expect(e),
            "filled-box" => settings.filled_box = value.get().expect(e),
            "tracking-outline-colors" => settings.tracking_outline_colors = value.get().expect(e),
            "defer-labels" => settings.defer_labels = value.get().expect(e),
            "priority" => settings.priority = value.get().expect(e),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "object-detection-outline-color" => settings.object_detection_outline_color.to_value(),
            "draw-labels" => settings.draw_labels.to_value(),
            "draw-tracking-labels" => settings.draw_tracking_labels.to_value(),
            "labels-color" => settings.labels_color.to_value(),
            "filled-box" => settings.filled_box.to_value(),
            "tracking-outline-colors" => settings.tracking_outline_colors.to_value(),
            "defer-labels" => settings.defer_labels.to_value(),
            "priority" => settings.priority.to_value(),
            _ => unimplemented!(),
        }
    }
}
impl GstObjectImpl for ObjectDetectionOverlayGl {}

impl ElementImpl for ObjectDetectionOverlayGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Object Detection Overlay",
                "Filter/Effect/Video/Visualization",
                "Draws object-detection overlays on GL textures via skia GPU",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*META)
    }
}

impl BaseTransformImpl for ObjectDetectionOverlayGl {
    const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    // Runs just before `filter_texture` on the same thread, and unlike
    // `filter_texture` it has the buffer — so we read the meta and build the
    // draw commands here, then replay them on the GPU in `filter_texture`.
    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let (width, height) = *self.dims.lock().unwrap();
        let (commands, deferred) = if width > 0 && height > 0 {
            let bounds = od::FrameBounds { width, height };
            let settings = *self.settings.lock().unwrap();
            // In defer mode `analytics_to_overlay` returns anchored commands plus
            // label intents; otherwise the labels are placed into `commands`.
            let (_frame, commands, deferred) = od::analytics_to_overlay(inbuf, settings, bounds);
            (commands, deferred)
        } else {
            (Vec::new(), Vec::new())
        };
        *self.pending.lock().unwrap() = commands;
        *self.pending_labels.lock().unwrap() = deferred;
        self.parent_before_transform(inbuf);
    }

    // `before_transform` (above) runs first and fills `pending`/`pending_labels`;
    // this runs next and yields the output buffer, so we publish our claims and
    // (in defer mode) our label intents here for the downstream compositor.
    fn prepare_output_buffer(
        &self,
        inbuf: InputBuffer,
    ) -> Result<PrepareOutputBufferSuccess, gst::FlowError> {
        let success = self.parent_prepare_output_buffer(inbuf)?;
        if let PrepareOutputBufferSuccess::Buffer(mut outbuf) = success {
            let commands = self.pending.lock().unwrap().clone();
            let deferred = self.pending_labels.lock().unwrap().clone();
            let priority = self.settings.lock().unwrap().priority;
            if let Some(buffer) = outbuf.get_mut() {
                if !commands.is_empty() {
                    crate::coordination::claim_commands(
                        buffer,
                        &commands,
                        od::OVERLAY_OWNER,
                        priority,
                    );
                }
                if !deferred.is_empty() {
                    add_label_intents(buffer, &deferred);
                }
            }
            return Ok(PrepareOutputBufferSuccess::Buffer(outbuf));
        }
        Ok(success)
    }
}

impl GLBaseFilterImpl for ObjectDetectionOverlayGl {
    fn gl_set_caps(
        &self,
        incaps: &gst::Caps,
        outcaps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        if let Ok(info) = gst_video::VideoInfo::from_caps(outcaps) {
            *self.dims.lock().unwrap() = (info.width() as i32, info.height() as i32);
        }
        self.parent_gl_set_caps(incaps, outcaps)
    }

    fn gl_start(&self) -> Result<(), gst::LoggableError> {
        let context = GLBaseFilterExt::context(&*self.obj())
            .ok_or_else(|| gst::loggable_error!(CAT, "no GL context"))?;
        let gpu = GpuState::new(&context)
            .ok_or_else(|| gst::loggable_error!(CAT, "failed to create skia GL context"))?;

        gst::info!(CAT, imp = self, "skia GPU context created");
        *self.gpu.lock().unwrap() = Some(gpu);
        self.parent_gl_start()
    }

    fn gl_stop(&self) {
        *self.gpu.lock().unwrap() = None;
        self.parent_gl_stop()
    }
}

impl GLFilterImpl for ObjectDetectionOverlayGl {
    const MODE: GLFilterMode = GLFilterMode::Texture;

    fn filter_texture(
        &self,
        input: &gst_gl::GLMemory,
        output: &gst_gl::GLMemory,
    ) -> Result<(), gst::LoggableError> {
        let commands = self.pending.lock().unwrap().clone();

        let mut guard = self.gpu.lock().unwrap();
        let Some(gpu) = guard.as_mut() else {
            return Err(gst::loggable_error!(CAT, "no GPU context"));
        };
        if gpu.render_to_texture(input, output, "od", |canvas| {
            replay_commands(canvas, &commands);
        }) {
            Ok(())
        } else {
            Err(gst::loggable_error!(CAT, "wrap_backend_texture failed"))
        }
    }
}
