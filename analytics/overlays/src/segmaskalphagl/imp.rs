// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GL variant of `segmaskalpha`: writes an analytics segmentation mask into a GL
//! texture's alpha channel via skia's Ganesh backend.
//!
//! The alpha map is built on the CPU (reusing [`crate::segmaskalpha::alpha`],
//! which also reads the CPU-side Gray8 mask buffer), uploaded as an 8-bit alpha
//! (A8) skia image in `before_transform`, and applied in `filter_texture`: the
//! input texture is drawn through, then the A8 mask is composited with
//! [`skia::BlendMode::DstIn`], whose result alpha is `dst.a × mask` — i.e. it
//! replaces the (opaque) frame's alpha with the mask, leaving colour intact.
//! The expensive full-frame blur/composite then stays on the GPU downstream (see
//! the `segbackgroundblurgl` bin).

use gst::glib;
use gst::subclass::prelude::*;
use gst_base::subclass::BaseTransformMode;
use gst_base::subclass::prelude::*;
use gst_gl::prelude::*;
use gst_gl::subclass::GLFilterMode;
use gst_gl::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

use crate::glsupport::GpuState;
use crate::segmaskalpha::alpha::build_frame_alpha;
use crate::segmentationoverlay::masks::State;

const DEFAULT_INVERT: bool = false;
const DEFAULT_FEATHER: u32 = 0;
const MAX_FEATHER: u32 = 128;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segmaskalphagl",
        gst::DebugColorFlags::empty(),
        Some("GL segmentation mask to alpha (skia GPU)"),
    )
});

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
pub struct SegMaskAlphaGl {
    gpu: Mutex<Option<GpuState>>,
    dims: Mutex<(i32, i32)>,
    // Reused only for its `selected-types` class-filter cache.
    state: Mutex<State>,
    settings: Mutex<Settings>,
    /// A8 alpha image for the current buffer, built in `before_transform`; `None`
    /// when the buffer has no analytics meta (frame then passes through).
    pending: Mutex<Option<skia::Image>>,
}

#[glib::object_subclass]
impl ObjectSubclass for SegMaskAlphaGl {
    const NAME: &'static str = "GstRsSegMaskAlphaGl";
    type Type = super::SegMaskAlphaGl;
    type ParentType = gst_gl::GLFilter;
}

impl ObjectImpl for SegMaskAlphaGl {
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
                    .blurb("When true the object is transparent instead of opaque")
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

impl GstObjectImpl for SegMaskAlphaGl {}

impl ElementImpl for SegMaskAlphaGl {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "GL Segmentation Mask to Alpha",
                "Filter/Effect/Video",
                "Writes an analytics segmentation mask into a GL texture's alpha channel \
                 via skia GPU, for background blur/replace",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*META)
    }
}

impl BaseTransformImpl for SegMaskAlphaGl {
    const MODE: BaseTransformMode = BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let (w, h) = *self.dims.lock().unwrap();
        let mut image = None;
        if w > 0 && h > 0 {
            let settings = self.settings.lock().unwrap().clone();
            let mut state = self.state.lock().unwrap();
            if let Some(mut alpha) = build_frame_alpha(
                &mut state,
                settings.selected_types.as_deref(),
                settings.feather,
                inbuf,
                w as usize,
                h as usize,
            ) {
                if settings.invert {
                    for a in alpha.iter_mut() {
                        *a = 255 - *a;
                    }
                }
                let info = skia::ImageInfo::new(
                    skia::ISize::new(w, h),
                    skia::ColorType::Alpha8,
                    skia::AlphaType::Unpremul,
                    None,
                );
                let data = skia::Data::new_copy(&alpha);
                image = skia::images::raster_from_data(&info, data, w as usize);
            }
        }
        *self.pending.lock().unwrap() = image;
        self.parent_before_transform(inbuf);
    }
}

impl GLBaseFilterImpl for SegMaskAlphaGl {
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
        *self.gpu.lock().unwrap() = Some(gpu);
        self.parent_gl_start()
    }

    fn gl_stop(&self) {
        *self.gpu.lock().unwrap() = None;
        self.parent_gl_stop()
    }
}

impl GLFilterImpl for SegMaskAlphaGl {
    const MODE: GLFilterMode = GLFilterMode::Texture;

    fn filter_texture(
        &self,
        input: &gst_gl::GLMemory,
        output: &gst_gl::GLMemory,
    ) -> Result<(), gst::LoggableError> {
        let image = self.pending.lock().unwrap().take();

        let mut guard = self.gpu.lock().unwrap();
        let Some(gpu) = guard.as_mut() else {
            return Err(gst::loggable_error!(CAT, "no GPU context"));
        };
        let drawn = gpu.render_to_texture(input, output, "segmaskalpha", |canvas| {
            // The input has already been drawn through; DstIn keeps its colour and
            // sets alpha = input.alpha × mask. With no mask, leave it untouched.
            if let Some(image) = &image {
                let mut paint = skia::Paint::default();
                paint.set_blend_mode(skia::BlendMode::DstIn);
                canvas.draw_image(image, (0, 0), Some(&paint));
            }
        });
        if drawn {
            Ok(())
        } else {
            Err(gst::loggable_error!(CAT, "wrap_backend_texture failed"))
        }
    }
}
