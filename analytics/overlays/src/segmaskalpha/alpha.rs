// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Shared segmentation-mask → alpha-map builder, used by the CPU element
//! (`segmaskalpha`) and the GL element (`segmaskalphagl`). Produces a full-frame
//! 8-bit alpha map (255 = selected object, 0 = background) from every
//! segmentation mask on a buffer, reusing the segmentation overlay's mask access
//! and `selected-types` class filtering.

use gst_analytics::{AnalyticsMetaRefExt, AnalyticsRelationMeta};
use gst_video::prelude::VideoFrameExt;

use crate::segmentationoverlay::masks::{
    AnalyticsSegmentationMtd, SegmentationMtdExt, State, cached_mask_filter,
    related_classification, update_selected_type_cache,
};

/// Bilinearly sample a `w`×`h` byte map at fractional `(sx, sy)`, clamping to the
/// edges. Turns the mask's hard 0/255 native-resolution edge into a smooth ramp
/// when upscaled into the frame.
fn sample_bilinear(map: &[u8], w: usize, h: usize, sx: f32, sy: f32) -> u8 {
    let sx = sx.clamp(0.0, (w - 1) as f32);
    let sy = sy.clamp(0.0, (h - 1) as f32);
    let x0 = sx.floor() as usize;
    let y0 = sy.floor() as usize;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = sx - x0 as f32;
    let fy = sy - y0 as f32;

    let p00 = map[y0 * w + x0] as f32;
    let p10 = map[y0 * w + x1] as f32;
    let p01 = map[y1 * w + x0] as f32;
    let p11 = map[y1 * w + x1] as f32;

    let top = p00 + (p10 - p00) * fx;
    let bottom = p01 + (p11 - p01) * fx;
    (top + (bottom - top) * fy).round().clamp(0.0, 255.0) as u8
}

/// Separable box blur of `alpha` (a `w`×`h` map) with the given radius; two
/// passes (horizontal then vertical) of a moving average. `tmp` is a reusable
/// scratch buffer (resized to `alpha.len()`) so the blur allocates nothing per
/// call.
fn box_blur(alpha: &mut [u8], tmp: &mut Vec<u8>, w: usize, h: usize, radius: usize) {
    if radius == 0 {
        return;
    }
    let window = (radius * 2 + 1) as u32;

    tmp.clear();
    tmp.resize(alpha.len(), 0);
    for y in 0..h {
        let row = &alpha[y * w..y * w + w];
        let mut sum: u32 = 0;
        for &v in &row[..radius.min(w)] {
            sum += v as u32;
        }
        for x in 0..w {
            let add = (x + radius).min(w - 1);
            let sub = x.wrapping_sub(radius + 1);
            sum += row[add] as u32;
            if x > radius {
                sum -= row[sub] as u32;
            }
            tmp[y * w + x] = (sum / window) as u8;
        }
    }

    for x in 0..w {
        let mut sum: u32 = 0;
        for y in 0..radius.min(h) {
            sum += tmp[y * w + x] as u32;
        }
        for y in 0..h {
            let add = (y + radius).min(h - 1);
            sum += tmp[add * w + x] as u32;
            if y > radius {
                sum -= tmp[(y - radius - 1) * w + x] as u32;
            }
            alpha[y * w + x] = (sum / window) as u8;
        }
    }
}

/// Build a full-frame alpha map (255 = object, 0 = background) from every
/// segmentation mask on `buffer`, into `state`'s reusable scratch buffer.
/// Returns a borrow of that buffer, or `None` when there is no analytics meta (or
/// no usable mask) so the caller can leave the frame's alpha as it arrived.
/// `state` also carries the shared `selected-types` class-filter cache.
///
/// The full-frame alpha and blur-temp buffers are reused across frames (held in
/// `state`), so a per-frame call allocates only the small native-resolution
/// object map.
pub(crate) fn build_frame_alpha<'s>(
    state: &'s mut State,
    selected_types: Option<&str>,
    feather: u32,
    buffer: &gst::BufferRef,
    frame_w: usize,
    frame_h: usize,
) -> Option<&'s mut [u8]> {
    update_selected_type_cache(state, selected_types);
    let selected_types = state.selected_type_quarks();

    let meta = buffer.meta::<AnalyticsRelationMeta>()?;

    // Reuse the frame-sized scratch buffer, cleared to background.
    state.alpha_scratch.clear();
    state.alpha_scratch.resize(frame_w * frame_h, 0);
    let mut any = false;

    for seg_mtd in meta.iter::<AnalyticsSegmentationMtd>() {
        let Some((mask, ofx, ofy, dst_w, dst_h)) = seg_mtd.mask() else {
            continue;
        };
        if dst_w == 0 || dst_h == 0 {
            continue;
        }

        // Read the native-resolution Gray8 mask.
        let Some(mask_meta) = mask.meta::<gst_video::VideoMeta>() else {
            continue;
        };
        let Ok(mask_info) = gst_video::VideoInfo::builder(
            mask_meta.format(),
            mask_meta.width(),
            mask_meta.height(),
        )
        .build() else {
            continue;
        };
        let Ok(mask_frame) = gst_video::VideoFrame::from_buffer_readable(mask, &mask_info) else {
            continue;
        };
        let mask_w = mask_frame.width() as usize;
        let mask_h = mask_frame.height() as usize;
        if mask_w == 0 || mask_h == 0 {
            continue;
        }
        let Ok(mask_data) = mask_frame.plane_data(0) else {
            continue;
        };
        let mask_stride = mask_frame.plane_stride()[0].unsigned_abs() as usize;

        // Compute the class filter (borrows `state`) and release it before
        // borrowing the scratch buffer below.
        let cls_mtd = related_classification(&meta, &seg_mtd);
        let mask_filter = cached_mask_filter(state, cls_mtd.as_ref(), selected_types.as_deref());
        let mask_filter = mask_filter.as_deref();

        // Native binary object map: 255 where the mask value is non-zero and
        // passes the class filter, else 0. This is small (native mask resolution)
        // so it is not worth caching across frames.
        let mut objmap = vec![0u8; mask_w * mask_h];
        for y in 0..mask_h {
            let row = &mask_data[y * mask_stride..y * mask_stride + mask_w];
            for x in 0..mask_w {
                let value = row[x] as usize;
                let allowed = mask_filter
                    .map(|filter| value < filter.len() && filter[value])
                    .unwrap_or(true);
                if value != 0 && allowed {
                    objmap[y * mask_w + x] = 255;
                }
            }
        }

        // Bilinearly upscale into the frame region the mask covers, taking the
        // max so overlapping objects union.
        let scale_x = mask_w as f32 / dst_w as f32;
        let scale_y = mask_h as f32 / dst_h as f32;
        let start_x = ofx.max(0);
        let start_y = ofy.max(0);
        let end_x = (ofx + dst_w as i32).min(frame_w as i32);
        let end_y = (ofy + dst_h as i32).min(frame_h as i32);

        let alpha = &mut state.alpha_scratch;
        for fy in start_y..end_y {
            let sy = (fy - ofy) as f32 * scale_y;
            for fx in start_x..end_x {
                let sx = (fx - ofx) as f32 * scale_x;
                let a = sample_bilinear(&objmap, mask_w, mask_h, sx, sy);
                let slot = &mut alpha[fy as usize * frame_w + fx as usize];
                *slot = (*slot).max(a);
            }
        }
        any = true;
    }

    if !any {
        return None;
    }

    if feather > 0 {
        box_blur(
            &mut state.alpha_scratch,
            &mut state.blur_scratch,
            frame_w,
            frame_h,
            feather as usize,
        );
    }

    Some(&mut state.alpha_scratch)
}
