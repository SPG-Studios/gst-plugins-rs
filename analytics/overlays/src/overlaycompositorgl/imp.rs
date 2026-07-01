// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// GL deferred-label compositor: the GPU sibling of `overlaycompositor`. Reads the
// claims + deferred label intents on the buffer, places the labels globally by
// priority (CPU geometry via `placement::place_labels`), and renders them onto
// the pipeline's GL texture with skia's Ganesh backend. The claims/intents are
// read in `before_transform` (which has the buffer); the GPU draw happens in
// `filter_texture`.

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::subclass::BaseTransformMode;
use gst_base::subclass::prelude::*;
use gst_gl::prelude::*;
use gst_gl::subclass::GLFilterMode;
use gst_gl::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use crate::coordination::claimed_regions;
use crate::glsupport::GpuState;
use crate::overlay_intent::label_intents;
use crate::placement::place_labels;
use crate::render::{DrawCommand, replay_commands};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "overlaycompositorgl",
        gst::DebugColorFlags::empty(),
        Some("GL deferred overlay-label compositor (skia GPU)"),
    )
});

#[derive(Default)]
pub struct OverlayCompositorGl {
    gpu: Mutex<Option<GpuState>>,
    /// Negotiated frame size (width, height), from `gl_set_caps`.
    dims: Mutex<(i32, i32)>,
    /// Placed label draw commands for the current buffer, from `before_transform`.
    pending: Mutex<Vec<DrawCommand>>,
}

#[glib::object_subclass]
impl ObjectSubclass for OverlayCompositorGl {
    const NAME: &'static str = "GstRsOverlayCompositorGl";
    type Type = super::OverlayCompositorGl;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for OverlayCompositorGl {}
impl GstObjectImpl for OverlayCompositorGl {}

impl ElementImpl for OverlayCompositorGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Overlay Label Compositor",
                "Filter/Effect/Video/Visualization",
                "Places and renders deferred overlay labels on GL textures via skia GPU",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*META)
    }
}

impl BaseTransformImpl for OverlayCompositorGl {
    const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    // Runs before `filter_texture` with the buffer, so we read the claims +
    // intents and place the labels here, then replay them on the GPU.
    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let (width, height) = *self.dims.lock().unwrap();
        let commands = if width > 0 && height > 0 {
            let claims = claimed_regions(inbuf);
            let intents = label_intents(inbuf);
            place_labels(width, height, &claims, &intents)
        } else {
            Vec::new()
        };
        *self.pending.lock().unwrap() = commands;
        self.parent_before_transform(inbuf);
    }
}

impl GLBaseFilterImpl for OverlayCompositorGl {
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

impl GLFilterImpl for OverlayCompositorGl {
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
        if gpu.render_to_texture(input, output, "compose", |canvas| {
            replay_commands(canvas, &commands);
        }) {
            Ok(())
        } else {
            Err(gst::loggable_error!(CAT, "wrap_backend_texture failed"))
        }
    }
}
