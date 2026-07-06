// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// GL keypoints overlay. Reads AnalyticsRelationMeta and renders markers +
// skeleton + labels with skia's Ganesh GPU backend onto the pipeline's GL
// textures, reusing the CPU element's command generation and the shared
// `render::replay_commands`. The buffer (for the meta) is captured in
// `before_transform`; the GPU draw happens in `filter_texture`.

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
use crate::keypointsoverlay::commands as kp;
use crate::overlay_intent::{LabelIntent, add_label_intents};
use crate::render::{DrawCommand, replay_commands};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "keypointsoverlaygl",
        gst::DebugColorFlags::empty(),
        Some("GL keypoints overlay (skia GPU)"),
    )
});

#[derive(Default)]
pub struct KeypointsOverlayGl {
    gpu: Mutex<Option<GpuState>>,
    dims: Mutex<(i32, i32)>,
    pending: Mutex<Vec<DrawCommand>>,
    /// Deferred label intents for the current buffer (defer mode), emitted onto
    /// the output buffer in `prepare_output_buffer`.
    pending_labels: Mutex<Vec<LabelIntent>>,
    /// Rendering settings (the CPU element's `Settings`, reused), driven by the
    /// element's GObject properties.
    settings: Mutex<kp::Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for KeypointsOverlayGl {
    const NAME: &'static str = "GstRsKeypointsOverlayGl";
    type Type = super::KeypointsOverlayGl;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for KeypointsOverlayGl {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let d = kp::Settings::default();
            vec![
                glib::ParamSpecUInt::builder("keypoint-color")
                    .nick("Keypoint color")
                    .blurb("Color used to draw keypoints")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(d.keypoint_color)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("keypoint-radius")
                    .nick("Keypoint radius")
                    .blurb("Radius in pixels used for keypoints")
                    .minimum(1.0)
                    .maximum(20.0)
                    .default_value(d.keypoint_radius)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-labels")
                    .nick("Draw labels")
                    .blurb("Draw keypoint confidence labels")
                    .default_value(d.draw_labels)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("labels-color")
                    .nick("Labels color")
                    .blurb("Color used for keypoint labels")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(d.labels_color)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("draw-skeleton")
                    .nick("Draw skeleton")
                    .blurb("Draw skeleton relations between keypoints")
                    .default_value(d.draw_skeleton)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("skeleton-color")
                    .nick("Skeleton color")
                    .blurb("Color used for skeleton lines")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(d.skeleton_color)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("skeleton-line-width")
                    .nick("Skeleton line width")
                    .blurb("Line width in pixels for skeleton rendering")
                    .minimum(1.0)
                    .maximum(10.0)
                    .default_value(d.skeleton_line_width)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("semantic-tag")
                    .nick("Semantic tag")
                    .blurb("Semantic tag prefix used to filter grouped keypoints")
                    .default_value(d.semantic_tag.as_deref())
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
                crate::coordination::publish_claimed_regions_param_spec(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        let e = "type checked upstream";
        match pspec.name() {
            "keypoint-color" => settings.keypoint_color = value.get().expect(e),
            "keypoint-radius" => settings.keypoint_radius = value.get().expect(e),
            "draw-labels" => settings.draw_labels = value.get().expect(e),
            "labels-color" => settings.labels_color = value.get().expect(e),
            "draw-skeleton" => settings.draw_skeleton = value.get().expect(e),
            "skeleton-color" => settings.skeleton_color = value.get().expect(e),
            "skeleton-line-width" => settings.skeleton_line_width = value.get().expect(e),
            "semantic-tag" => settings.semantic_tag = value.get().expect(e),
            "defer-labels" => settings.defer_labels = value.get().expect(e),
            "priority" => settings.priority = value.get().expect(e),
            "publish-claimed-regions" => settings.publish_claimed_regions = value.get().expect(e),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "keypoint-color" => settings.keypoint_color.to_value(),
            "keypoint-radius" => settings.keypoint_radius.to_value(),
            "draw-labels" => settings.draw_labels.to_value(),
            "labels-color" => settings.labels_color.to_value(),
            "draw-skeleton" => settings.draw_skeleton.to_value(),
            "skeleton-color" => settings.skeleton_color.to_value(),
            "skeleton-line-width" => settings.skeleton_line_width.to_value(),
            "semantic-tag" => settings.semantic_tag.to_value(),
            "defer-labels" => settings.defer_labels.to_value(),
            "priority" => settings.priority.to_value(),
            "publish-claimed-regions" => settings.publish_claimed_regions.to_value(),
            _ => unimplemented!(),
        }
    }
}
impl GstObjectImpl for KeypointsOverlayGl {}

impl ElementImpl for KeypointsOverlayGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Keypoints Overlay",
                "Filter/Effect/Video/Visualization",
                "Draws keypoint markers + skeleton on GL textures via skia GPU",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*META)
    }
}

impl BaseTransformImpl for KeypointsOverlayGl {
    const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let (width, height) = *self.dims.lock().unwrap();
        let (commands, deferred) = if width > 0 && height > 0 {
            let bounds = kp::FrameBounds { width, height };
            let settings = self.settings.lock().unwrap().clone();
            // In defer mode `analytics_to_overlay` returns anchored commands plus
            // label intents; otherwise the labels are placed into `commands`.
            let (_frame, commands, deferred) = kp::analytics_to_overlay(inbuf, &settings, bounds);
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
            let (priority, publish_claimed_regions) = {
                let settings = self.settings.lock().unwrap();
                (settings.priority, settings.publish_claimed_regions)
            };
            if let Some(buffer) = outbuf.get_mut() {
                if publish_claimed_regions && !commands.is_empty() {
                    crate::coordination::claim_commands(
                        buffer,
                        &commands,
                        kp::OVERLAY_OWNER,
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

impl GLBaseFilterImpl for KeypointsOverlayGl {
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

impl GLFilterImpl for KeypointsOverlayGl {
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
        if gpu.render_to_texture(input, output, "kp", |canvas| {
            replay_commands(canvas, &commands);
        }) {
            Ok(())
        } else {
            Err(gst::loggable_error!(CAT, "wrap_backend_texture failed"))
        }
    }
}
