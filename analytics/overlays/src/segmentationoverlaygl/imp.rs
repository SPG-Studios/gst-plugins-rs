// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// GL segmentation overlay. Reads AnalyticsRelationMeta, colorizes each mask at
// native resolution on the CPU (reusing the CPU element's
// `segmentation_mask_layers`), then uploads and composites the layers with
// skia's Ganesh GPU backend — the GPU does the full-frame scale + alpha blend,
// which is where the big win is. Masks are colorized in `before_transform`; the
// GPU compositing happens in `filter_texture`.

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::subclass::BaseTransformMode;
use gst_base::subclass::base_transform::{InputBuffer, PrepareOutputBufferSuccess};
use gst_base::subclass::prelude::*;
use gst_gl::prelude::*;
use gst_gl::subclass::GLFilterMode;
use gst_gl::subclass::prelude::*;

use std::ffi::c_void;
use std::sync::{LazyLock, Mutex};

use skia::gpu;

use crate::coordination::{ClaimedRegion, add_claimed_regions};
use crate::geometry::Rect;
use crate::segmentationoverlay::masks as seg;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segoverlaygl",
        gst::DebugColorFlags::empty(),
        Some("GL segmentation overlay (skia GPU)"),
    )
});

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA8: u32 = 0x8058;

/// A colorized mask uploaded as a skia image, with its destination rect (in skia
/// coordinates for compositing, and in frame coordinates for claiming).
struct PendingLayer {
    image: skia::Image,
    dst: skia::Rect,
    claim: Rect,
}

struct GpuState {
    context: gpu::DirectContext,
}
unsafe impl Send for GpuState {}

#[derive(Default)]
pub struct SegmentationOverlayGl {
    gpu: Mutex<Option<GpuState>>,
    dims: Mutex<(i32, i32)>,
    state: Mutex<seg::State>,
    /// Colorized mask layers for the current buffer, built in `before_transform`.
    pending: Mutex<Vec<PendingLayer>>,
    /// Rendering settings (the CPU element's `Settings`, reused), driven by the
    /// element's GObject properties.
    settings: Mutex<seg::Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for SegmentationOverlayGl {
    const NAME: &'static str = "GstRsSegmentationOverlayGl";
    type Type = super::SegmentationOverlayGl;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for SegmentationOverlayGl {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let d = seg::Settings::default();
            vec![
                glib::ParamSpecUInt::builder("hint-maximum-segment-type")
                    .nick("Hint maximum segment type")
                    .blurb("Hint for expected maximum segment type value")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(d.hint_maximum_segment_type)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("selected-types")
                    .nick("Selected types")
                    .blurb("Semicolon-separated type names to render")
                    .default_value(d.selected_types.as_deref())
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        let e = "type checked upstream";
        match pspec.name() {
            "hint-maximum-segment-type" => {
                settings.hint_maximum_segment_type = value.get().expect(e)
            }
            "selected-types" => settings.selected_types = value.get().expect(e),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "hint-maximum-segment-type" => settings.hint_maximum_segment_type.to_value(),
            "selected-types" => settings.selected_types.to_value(),
            _ => unimplemented!(),
        }
    }
}
impl GstObjectImpl for SegmentationOverlayGl {}

impl ElementImpl for SegmentationOverlayGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Segmentation Overlay",
                "Filter/Effect/Video/Visualization",
                "Composites segmentation masks on GL textures via skia GPU",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*META)
    }
}

impl BaseTransformImpl for SegmentationOverlayGl {
    const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let (width, height) = *self.dims.lock().unwrap();
        let mut layers = Vec::new();
        if width > 0 && height > 0 {
            let mut state = self.state.lock().unwrap();
            let settings = self.settings.lock().unwrap().clone();
            let masks = seg::segmentation_mask_layers(inbuf, &settings, &mut state, width, height);
            // Convert each native-res BGRA canvas into a (raster) skia image,
            // uploaded to the GPU lazily on first draw.
            for m in masks {
                let Ok(map) = m.canvas.into_mapped_buffer_readable() else {
                    continue;
                };
                let info = skia::ImageInfo::new(
                    skia::ISize::new(m.width as i32, m.height as i32),
                    skia::ColorType::BGRA8888,
                    skia::AlphaType::Premul,
                    None,
                );
                let data = skia::Data::new_copy(map.as_slice());
                if let Some(image) =
                    skia::images::raster_from_data(&info, data, m.width as usize * 4)
                {
                    layers.push(PendingLayer {
                        image,
                        dst: skia::Rect::from_xywh(
                            m.dst_x as f32,
                            m.dst_y as f32,
                            m.dst_w as f32,
                            m.dst_h as f32,
                        ),
                        claim: Rect::from_xywh(m.dst_x, m.dst_y, m.dst_w as i32, m.dst_h as i32),
                    });
                }
            }
        }
        *self.pending.lock().unwrap() = layers;
        self.parent_before_transform(inbuf);
    }

    // `before_transform` (above) runs first and fills `pending`; this runs next
    // and yields the output buffer, so we publish the mask regions as soft Avoid
    // claims here (mirroring the CPU element) for downstream overlays to avoid.
    fn prepare_output_buffer(
        &self,
        inbuf: InputBuffer,
    ) -> Result<PrepareOutputBufferSuccess, gst::FlowError> {
        let success = self.parent_prepare_output_buffer(inbuf)?;
        if let PrepareOutputBufferSuccess::Buffer(mut outbuf) = success {
            let regions: Vec<ClaimedRegion> = self
                .pending
                .lock()
                .unwrap()
                .iter()
                .map(|layer| ClaimedRegion::avoid(layer.claim, seg::OVERLAY_OWNER))
                .collect();
            if let (false, Some(buffer)) = (regions.is_empty(), outbuf.get_mut()) {
                add_claimed_regions(buffer, &regions);
            }
            return Ok(PrepareOutputBufferSuccess::Buffer(outbuf));
        }
        Ok(success)
    }
}

impl GLBaseFilterImpl for SegmentationOverlayGl {
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
        let filter = self.obj();
        let context = GLBaseFilterExt::context(&*filter)
            .ok_or_else(|| gst::loggable_error!(CAT, "no GL context"))?;

        let interface =
            gpu::gl::Interface::new_load_with(|name| context.proc_address(name) as *const c_void)
                .ok_or_else(|| gst::loggable_error!(CAT, "failed to create skia GL interface"))?;
        let gr = gpu::direct_contexts::make_gl(interface, None)
            .ok_or_else(|| gst::loggable_error!(CAT, "failed to create skia GL context"))?;

        gst::info!(CAT, imp = self, "skia GPU context created");
        *self.gpu.lock().unwrap() = Some(GpuState { context: gr });
        self.parent_gl_start()
    }

    fn gl_stop(&self) {
        *self.gpu.lock().unwrap() = None;
        self.parent_gl_stop()
    }
}

impl GLFilterImpl for SegmentationOverlayGl {
    const MODE: GLFilterMode = GLFilterMode::Texture;

    fn filter_texture(
        &self,
        input: &gst_gl::GLMemory,
        output: &gst_gl::GLMemory,
    ) -> Result<(), gst::LoggableError> {
        let layers = std::mem::take(&mut *self.pending.lock().unwrap());

        let mut guard = self.gpu.lock().unwrap();
        let Some(state) = guard.as_mut() else {
            return Err(gst::loggable_error!(CAT, "no GPU context"));
        };
        let ctx = &mut state.context;

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
            gpu::backend_textures::make_gl((width, height), gpu::Mipmapped::No, out_info, "seg-out")
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
            return Err(gst::loggable_error!(CAT, "wrap_backend_texture failed"));
        };

        let in_info = gpu::gl::TextureInfo {
            target: GL_TEXTURE_2D,
            id: input.texture_id(),
            format: GL_RGBA8,
            protected: gpu::Protected::No,
        };
        let in_bt = unsafe {
            gpu::backend_textures::make_gl((width, height), gpu::Mipmapped::No, in_info, "seg-in")
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

        // Composite each colorized mask, GPU-scaling it to its destination rect.
        let paint = skia::Paint::default();
        for layer in &layers {
            canvas.draw_image_rect(&layer.image, None, layer.dst, &paint);
        }

        ctx.flush_and_submit();
        Ok(())
    }
}
