// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Shared GstGL + skia-Ganesh plumbing for the GL overlay elements and the GL
// compositor: building a skia GPU context from a `GstGLContext`, and the common
// "wrap the output texture, draw the input texture through, draw on top, flush"
// render path. Each GL element supplies only its own drawing via a closure.

use std::ffi::c_void;

use gst_gl::prelude::*;

use skia::gpu;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA8: u32 = 0x8058;

/// skia's `DirectContext` is not `Send`/`Sync`, but it is only ever touched on
/// the single GstGL streaming thread, so we assert it is safe to hold.
pub(crate) struct GpuState {
    context: gpu::DirectContext,
}

unsafe impl Send for GpuState {}

impl GpuState {
    /// Build a skia Ganesh GPU context from the pipeline's `GstGLContext`.
    pub(crate) fn new(gl_context: &gst_gl::GLContext) -> Option<Self> {
        let interface = gpu::gl::Interface::new_load_with(|name| {
            gl_context.proc_address(name) as *const c_void
        })?;
        let context = gpu::direct_contexts::make_gl(interface, None)?;
        Some(Self { context })
    }

    /// Wrap `output` as a GPU surface, draw the `input` texture through to it,
    /// invoke `draw` to render this element's content on top, then flush.
    /// `label` is a debug label for the backend textures. Returns `false` if the
    /// output texture could not be wrapped as a surface.
    pub(crate) fn render_to_texture<F: FnOnce(&skia::Canvas)>(
        &mut self,
        input: &gst_gl::GLMemory,
        output: &gst_gl::GLMemory,
        label: &str,
        draw: F,
    ) -> bool {
        let ctx = &mut self.context;
        let width = output.texture_width();
        let height = output.texture_height();

        ctx.reset(None);

        let out_info = gpu::gl::TextureInfo {
            target: GL_TEXTURE_2D,
            id: output.texture_id(),
            format: GL_RGBA8,
            protected: gpu::Protected::No,
        };
        let out_bt = unsafe {
            gpu::backend_textures::make_gl((width, height), gpu::Mipmapped::No, out_info, label)
        };
        let Some(mut surface) = gpu::surfaces::wrap_backend_texture(
            ctx,
            &out_bt,
            gpu::SurfaceOrigin::TopLeft,
            None,
            skia::ColorType::RGBA8888,
            None,
            None,
        ) else {
            return false;
        };

        let in_info = gpu::gl::TextureInfo {
            target: GL_TEXTURE_2D,
            id: input.texture_id(),
            format: GL_RGBA8,
            protected: gpu::Protected::No,
        };
        let in_bt = unsafe {
            gpu::backend_textures::make_gl((width, height), gpu::Mipmapped::No, in_info, label)
        };
        let canvas = surface.canvas();
        if let Some(image) = gpu::images::borrow_texture_from(
            ctx,
            &in_bt,
            gpu::SurfaceOrigin::TopLeft,
            skia::ColorType::RGBA8888,
            skia::AlphaType::Premul,
            None,
        ) {
            canvas.draw_image(&image, (0, 0), None);
        }

        draw(canvas);

        ctx.flush_and_submit();
        true
    }
}
