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
use gst_video::prelude::VideoFrameExt;

use gst_base::prelude::BaseTransformExt;
use gst_base::subclass::prelude::*;
use gst_video::subclass::prelude::*;

use crate::render::{DrawCommand, RenderContext};

use super::commands::{
    CAT, DEFAULT_DEFER_LABELS, DEFAULT_DRAW_LABELS, DEFAULT_DRAW_SKELETON, DEFAULT_KEYPOINT_COLOR,
    DEFAULT_KEYPOINT_RADIUS, DEFAULT_LABELS_COLOR, DEFAULT_RENDER_ENABLED, DEFAULT_SKELETON_COLOR,
    DEFAULT_SKELETON_LINE_WIDTH, DEFAULT_SUPPRESS_BUILTIN_RENDERING, FrameBounds, OVERLAY_OWNER,
    Settings, analytics_to_overlay,
};

use std::sync::{LazyLock, Mutex};

#[derive(Default)]
pub struct KeypointsOverlay {
    render_context: Mutex<RenderContext>,
    settings: Mutex<Settings>,
    draw_hooks: Mutex<crate::hooks::DrawHooks>,
}

impl KeypointsOverlay {
    pub(crate) fn set_pre_draw_hook(&self, hook: Box<dyn crate::hooks::DrawHook>) {
        self.draw_hooks.lock().unwrap().set_pre(hook);
    }

    pub(crate) fn set_post_draw_hook(&self, hook: Box<dyn crate::hooks::DrawHook>) {
        self.draw_hooks.lock().unwrap().set_post(hook);
    }

    pub(crate) fn clear_draw_hooks(&self) {
        self.draw_hooks.lock().unwrap().clear();
    }

    /// Wrap built-in commands with any host pre/post draw hooks. In suppression
    /// mode the built-ins are dropped, so the host's hooks fully replace them
    /// (the pre and post hooks still run).
    fn compose_with_hooks(
        &self,
        builtins: &[DrawCommand],
        width: i32,
        height: i32,
    ) -> Vec<DrawCommand> {
        let ctx = crate::hooks::DrawHookContext { width, height };
        let suppress = self.settings.lock().unwrap().suppress_builtin_rendering;
        let hooks = self.draw_hooks.lock().unwrap();
        // Only suppress when a hook is set, so suppression replaces built-ins
        // rather than silently blanking the overlay when nothing draws.
        let builtins: &[DrawCommand] = if suppress && hooks.has_hooks() {
            &[]
        } else {
            builtins
        };
        hooks.compose(builtins, &ctx)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for KeypointsOverlay {
    // "GstRs" prefix keeps it distinct from the C "GstKeypointOverlay" element
    // (the factory name stays "keypointsoverlay").
    const NAME: &'static str = "GstRsKeypointsOverlay";
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
                glib::ParamSpecBoolean::builder("suppress-builtin-rendering")
                    .nick("Suppress built-in rendering")
                    .blurb(
                        "Skip the element's own keypoints/labels so custom draw hooks fully replace them",
                    )
                    .default_value(DEFAULT_SUPPRESS_BUILTIN_RENDERING)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("defer-labels")
                    .nick("Defer labels")
                    .blurb(
                        "Emit labels as deferred intents for a downstream overlaycompositor \
                         (which relocates them globally by priority) instead of placing them \
                         here. Requires a compositor downstream, or the labels are not drawn.",
                    )
                    .default_value(DEFAULT_DEFER_LABELS)
                    .mutable_playing()
                    .build(),
                crate::coordination::priority_param_spec(),
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
            "suppress-builtin-rendering" => {
                let mut settings = self.settings.lock().unwrap();
                settings.suppress_builtin_rendering = value.get().expect("type checked upstream");
            }
            "priority" => {
                let mut settings = self.settings.lock().unwrap();
                settings.priority = value.get().expect("type checked upstream");
            }
            "defer-labels" => {
                let mut settings = self.settings.lock().unwrap();
                settings.defer_labels = value.get().expect("type checked upstream");
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
            "suppress-builtin-rendering" => {
                let settings = self.settings.lock().unwrap();
                settings.suppress_builtin_rendering.to_value()
            }
            "priority" => {
                let settings = self.settings.lock().unwrap();
                settings.priority.to_value()
            }
            "defer-labels" => {
                let settings = self.settings.lock().unwrap();
                settings.defer_labels.to_value()
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
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let bounds = FrameBounds {
            width: frame.width() as i32,
            height: frame.height() as i32,
        };
        let (analytics, commands, deferred_labels) =
            analytics_to_overlay(frame.buffer(), &settings, bounds);

        // Wrap the built-ins with any host-supplied pre/post draw hooks.
        let render_commands = self.compose_with_hooks(&commands, bounds.width, bounds.height);

        let mut render_context = self.render_context.lock().unwrap();
        render_context.render(frame, &analytics, &render_commands)?;
        drop(render_context);

        // Publish what we drew (built-ins only, not host hooks) so downstream
        // overlays avoid occluding it. When suppression is active the built-ins
        // are not rendered, so nothing is claimed (suppression only applies when
        // a hook replaces them).
        let builtins_suppressed =
            settings.suppress_builtin_rendering && self.draw_hooks.lock().unwrap().has_hooks();
        if !builtins_suppressed && !commands.is_empty() {
            // SAFETY: the frame is writable and uniquely borrowed here.
            let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
            crate::coordination::claim_commands(
                buffer,
                &commands,
                OVERLAY_OWNER,
                settings.priority,
            );
        }

        // In defer mode, hand our labels to a downstream compositor (we drew only
        // the anchored markers/skeleton above).
        if !deferred_labels.is_empty() {
            // SAFETY: the frame is writable and uniquely borrowed here.
            let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
            crate::overlay_intent::add_label_intents(buffer, &deferred_labels);
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
