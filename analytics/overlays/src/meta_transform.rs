// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Coordinate-aware registration for our pixel-space custom metas (claimed
// regions and deferred label intents), so their rects survive resolution and
// geometry changes between coordinating elements — a `videoscale`, an
// aspect-ratio change with letterbox borders, or a crop.
//
// gstreamer-rs's safe `CustomMeta::register_with_transform` drops the transform
// `data` pointer, so its closure only sees the transform's `Quark` — not the
// `GstVideoMetaTransform` / `GstVideoMetaTransformMatrix` needed to compute the
// new coordinates. We therefore register the custom meta ourselves via
// `gst_meta_register_custom` with a trampoline that forwards the data. The
// unsafe FFI lives here only; callers provide a safe closure that maps each rect
// through a `Rect -> Option<Rect>` function (`None` = the rect fell outside the
// new frame and the region should be dropped).
//
// Modelled on `GstVideoRegionOfInterestMeta`'s transform, handling the same
// transform types:
//   * matrix (`gst-video-matrix`) — the full affine map used by
//     `videoconvertscale`, covering scale, crop and letterbox border offsets;
//     rects are clipped to the output frame (dropped if fully outside);
//   * scale (`gst-video-scale`) — the simpler in/out-ratio map some scalers use
//     (e.g. `glcolorscale`), which never clips;
//   * copy (`gst-copy`) — a plain buffer copy; rects are carried verbatim.

use glib::translate::{IntoGlib, ToGlibPtr, from_glib};

use gst::BufferRef;
use gst::meta::CustomMeta;

use crate::geometry::Rect;

/// Ratio between a scaler's input and output frame dimensions, from the scale
/// meta transform's in/out `GstVideoInfo`.
#[derive(Clone, Copy)]
pub(crate) struct ScaleRatio {
    in_w: i32,
    in_h: i32,
    out_w: i32,
    out_h: i32,
}

impl ScaleRatio {
    unsafe fn from_transform(data: *const gst_video::ffi::GstVideoMetaTransform) -> Option<Self> {
        let transform = unsafe { data.as_ref()? };
        let in_info = unsafe { transform.in_info.as_ref()? };
        let out_info = unsafe { transform.out_info.as_ref()? };
        Some(Self {
            in_w: in_info.width,
            in_h: in_info.height,
            out_w: out_info.width,
            out_h: out_info.height,
        })
    }

    /// Rescale `rect` from the input to the output coordinate space. A pure scale
    /// never moves a rect outside the frame, so this never drops one.
    pub(crate) fn scale_rect(&self, rect: Rect) -> Rect {
        // i64 math to avoid overflow; guard against a zero input dimension.
        let sx = |v: i32| ((v as i64 * self.out_w as i64) / self.in_w.max(1) as i64) as i32;
        let sy = |v: i32| ((v as i64 * self.out_h as i64) / self.in_h.max(1) as i64) as i32;
        Rect::from_xywh(
            sx(rect.left),
            sy(rect.top),
            sx(rect.width()),
            sy(rect.height()),
        )
    }
}

/// Map `rect` through the video matrix transform (scale + crop + border offset),
/// clipping it to the output frame. Returns `None` if it clips away entirely.
unsafe fn matrix_map_rect(
    transform: *const gst_video::ffi::GstVideoMetaTransformMatrix,
    rect: Rect,
) -> Option<Rect> {
    let mut area = gst_video::ffi::GstVideoRectangle {
        x: rect.left,
        y: rect.top,
        w: rect.width(),
        h: rect.height(),
    };
    let kept: bool = unsafe {
        from_glib(
            gst_video::ffi::gst_video_meta_transform_matrix_rectangle_clipped(transform, &mut area),
        )
    };
    kept.then(|| Rect::from_xywh(area.x, area.y, area.w, area.h))
}

/// Register a custom meta whose pixel rects follow the frame's coordinate space
/// across scale/crop/border transforms.
///
/// `transform` is called to build the destination meta. It receives a `map`
/// function to run each source rect through: it returns the rect in the output
/// space, or `None` if the rect fell entirely outside the new frame (that region
/// should be dropped). `map` is the identity on a plain copy. The transform is
/// not called for unknown transform types (the meta is then dropped rather than
/// copied with stale coordinates). Idempotent.
pub(crate) fn register_rect_transform<F>(name: &str, tags: &[&str], transform: F)
where
    F: Fn(&CustomMeta, &mut BufferRef, &dyn Fn(Rect) -> Option<Rect>) -> bool
        + Send
        + Sync
        + 'static,
{
    if CustomMeta::is_registered(name) {
        return;
    }

    unsafe extern "C" fn trampoline<F>(
        dest: *mut gst::ffi::GstBuffer,
        meta: *mut gst::ffi::GstCustomMeta,
        _src: *mut gst::ffi::GstBuffer,
        type_: glib::ffi::GQuark,
        data: glib::ffi::gpointer,
        user_data: glib::ffi::gpointer,
    ) -> glib::ffi::gboolean
    where
        F: Fn(&CustomMeta, &mut BufferRef, &dyn Fn(Rect) -> Option<Rect>) -> bool
            + Send
            + Sync
            + 'static,
    {
        unsafe {
            let transform_type: glib::Quark = from_glib(type_);
            let matrix_type: glib::Quark =
                from_glib(gst_video::ffi::gst_video_meta_transform_matrix_get_quark());
            let scale_type: glib::Quark =
                from_glib(gst_video::ffi::gst_video_meta_transform_scale_get_quark());
            let copy_type = glib::Quark::from_str("gst-copy");

            let map: Box<dyn Fn(Rect) -> Option<Rect>> = if transform_type == matrix_type {
                if data.is_null() {
                    return false.into_glib();
                }
                // `matrix` is valid for this call; `map` is invoked synchronously
                // by `func` below, before the trampoline returns.
                let matrix = data as *const gst_video::ffi::GstVideoMetaTransformMatrix;
                Box::new(move |rect| matrix_map_rect(matrix, rect))
            } else if transform_type == scale_type {
                let ratio = match ScaleRatio::from_transform(data as *const _) {
                    Some(ratio) => ratio,
                    None => return false.into_glib(),
                };
                Box::new(move |rect| Some(ratio.scale_rect(rect)))
            } else if transform_type == copy_type {
                Box::new(Some)
            } else {
                // Unknown transform: not handled.
                return false.into_glib();
            };

            let func = &*(user_data as *const F);
            let src_meta = &*(meta as *const CustomMeta);
            let dest = BufferRef::from_mut_ptr(dest);
            func(src_meta, dest, &*map).into_glib()
        }
    }

    unsafe extern "C" fn free<F>(ptr: glib::ffi::gpointer) {
        unsafe {
            let _ = Box::from_raw(ptr as *mut F);
        }
    }

    unsafe {
        let name = name.to_glib_none();
        let tags = tags.to_glib_none();
        gst::ffi::gst_meta_register_custom(
            name.0,
            tags.0,
            Some(trampoline::<F>),
            Box::into_raw(Box::new(transform)) as glib::ffi::gpointer,
            Some(free::<F>),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_rect_doubles_on_2x_upscale() {
        let ratio = ScaleRatio {
            in_w: 100,
            in_h: 100,
            out_w: 200,
            out_h: 200,
        };
        assert_eq!(
            ratio.scale_rect(Rect::from_xywh(10, 20, 30, 40)),
            Rect::from_xywh(20, 40, 60, 80)
        );
    }

    #[test]
    fn scale_rect_handles_anisotropic_and_zero_input() {
        // Different x/y ratios.
        let ratio = ScaleRatio {
            in_w: 100,
            in_h: 50,
            out_w: 50,
            out_h: 200,
        };
        assert_eq!(
            ratio.scale_rect(Rect::from_xywh(20, 10, 40, 10)),
            Rect::from_xywh(10, 40, 20, 40)
        );

        // A zero input dimension must not divide by zero.
        let degenerate = ScaleRatio {
            in_w: 0,
            in_h: 0,
            out_w: 100,
            out_h: 100,
        };
        let _ = degenerate.scale_rect(Rect::from_xywh(1, 1, 1, 1));
    }
}
