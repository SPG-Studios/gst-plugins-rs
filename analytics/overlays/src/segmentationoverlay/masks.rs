// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//
// Shared segmentation mask generation: colorizes AnalyticsRelationMeta
// segmentation masks at native resolution into `MaskLayer`s. Used by both the
// CPU element (`imp`) and the GL element (`segmentationoverlaygl`).

use gst::glib;
use gst_analytics::{
    AnalyticsClassificationMtd, AnalyticsMetaRefExt, AnalyticsMtd, AnalyticsRelationMeta, RelTypes,
};

use glib::translate::{UnsafeFrom, from_glib_full};

use gst_video::prelude::VideoFrameExt;

use std::sync::Arc;

pub(crate) const DEFAULT_RENDER_ENABLED: bool = false;
pub(crate) const DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE: u32 = 10;
const MASK_ALPHA: u8 = 0x80;

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) render_enabled: bool,
    pub(crate) hint_maximum_segment_type: u32,
    pub(crate) selected_types: Option<String>,
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

/// Per-segment colours indexed directly by the mask's Gray8 value (0..=255).
///
/// This replaces a `HashMap<usize, u32>` whose default-SipHash lookup ran for
/// every output pixel and dominated the segmentation hot path (~21% of the
/// element's cost in profiling). Mask values are bytes, so a fixed 256-entry
/// table makes the per-pixel lookup a plain array index. Colours are still
/// assigned lazily in first-seen order (preserving the previous behaviour), so
/// only the storage changed, not the resulting colours.
struct SegmentColorTable([Option<u32>; 256]);

impl Default for SegmentColorTable {
    fn default() -> Self {
        Self([None; 256])
    }
}

impl SegmentColorTable {
    fn clear(&mut self) {
        self.0 = [None; 256];
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }
}

#[derive(Default)]
pub(crate) struct State {
    segment_colors: SegmentColorTable,
    next_color_index: u64,
    composition: Option<gst_video::VideoOverlayComposition>,
    attach_composition: bool,
    selected_types_source: Option<String>,
    selected_type_quarks: Option<Vec<glib::Quark>>,
    mask_filter_cache: Option<MaskFilterCache>,
}

impl State {
    /// Clear all per-stream runtime state (segment colours, cached composition,
    /// attach decision and mask-filter cache). Mirrors the CPU element's
    /// `reset_runtime_state`.
    pub(crate) fn reset(&mut self) {
        self.segment_colors.clear();
        self.next_color_index = 0;
        self.composition = None;
        self.attach_composition = false;
        self.mask_filter_cache = None;
    }

    pub(crate) fn selected_type_quarks(&self) -> Option<Vec<glib::Quark>> {
        self.selected_type_quarks.clone()
    }

    pub(crate) fn composition(&self) -> Option<gst_video::VideoOverlayComposition> {
        self.composition.clone()
    }

    pub(crate) fn set_composition(
        &mut self,
        composition: Option<gst_video::VideoOverlayComposition>,
    ) {
        self.composition = composition;
    }

    pub(crate) fn attach_composition(&self) -> bool {
        self.attach_composition
    }

    pub(crate) fn set_attach_composition(&mut self, attach: bool) {
        self.attach_composition = attach;
    }

    #[cfg(test)]
    pub(crate) fn test_set_segment_color(&mut self, segment_value: usize, color: u32) {
        self.segment_colors.0[segment_value] = Some(color);
    }

    #[cfg(test)]
    pub(crate) fn test_segment_colors_is_empty(&self) -> bool {
        self.segment_colors.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn test_next_color_index(&self) -> u64 {
        self.next_color_index
    }

    #[cfg(test)]
    pub(crate) fn test_set_next_color_index(&mut self, index: u64) {
        self.next_color_index = index;
    }
}

struct MaskFilterCache {
    selected_types: Option<Vec<glib::Quark>>,
    cls_quarks: Vec<glib::Quark>,
    filter: Arc<[bool]>,
}

#[derive(Debug)]
pub(crate) enum AnalyticsSegmentationMtd {}

unsafe impl AnalyticsMtd for AnalyticsSegmentationMtd {
    fn mtd_type() -> gst_analytics::ffi::GstAnalyticsMtdType {
        unsafe { gst_analytics::ffi::gst_analytics_segmentation_mtd_get_mtd_type() }
    }
}

pub(crate) trait SegmentationMtdExt {
    fn mask(&self) -> Option<(gst::Buffer, i32, i32, u32, u32)>;
}

impl SegmentationMtdExt for gst_analytics::AnalyticsMtdRef<'_, AnalyticsSegmentationMtd> {
    fn mask(&self) -> Option<(gst::Buffer, i32, i32, u32, u32)> {
        let mut x = 0;
        let mut y = 0;
        let mut w = 0;
        let mut h = 0;

        let mask_ptr = unsafe {
            let mtd = gst_analytics::ffi::GstAnalyticsMtd::unsafe_from(self);
            gst_analytics::ffi::gst_analytics_segmentation_mtd_get_mask(
                &mtd as *const _ as *const gst_analytics::ffi::GstAnalyticsSegmentationMtd,
                &mut x,
                &mut y,
                &mut w,
                &mut h,
            )
        };

        if mask_ptr.is_null() {
            None
        } else {
            Some((unsafe { from_glib_full(mask_ptr) }, x, y, w, h))
        }
    }
}

fn hue_to_rgb(mut hue: f64) -> u32 {
    hue %= 360.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    let x = ((1.0 - ((hue / 60.0) % 2.0 - 1.0).abs()) * 255.0).round() as u32;

    if (0.0..60.0).contains(&hue) {
        (255 << 16) | (x << 8)
    } else if (60.0..120.0).contains(&hue) {
        (x << 16) | (255 << 8)
    } else if (120.0..180.0).contains(&hue) {
        (255 << 8) | x
    } else if (180.0..240.0).contains(&hue) {
        (x << 8) | 255
    } else if (240.0..300.0).contains(&hue) {
        (x << 16) | 255
    } else {
        (255 << 16) | x
    }
}

fn generate_segment_color(color_index: u64, hint_maximum_segment_type: u32) -> u32 {
    let hint = hint_maximum_segment_type.max(1) as f64;
    let seed = 360.0 / hint;
    let hue = (seed + color_index as f64 * 137.507_764_050_037_85) % 360.0;
    hue_to_rgb(hue)
}

pub(crate) fn color_for_segment(
    state: &mut State,
    segment_value: usize,
    hint_maximum_segment_type: u32,
) -> Option<u32> {
    if segment_value == 0 {
        return None;
    }

    // Direct array index (mask values are bytes); `None` for any out-of-range
    // value. Colours are assigned lazily on first appearance, as before.
    let slot = state.segment_colors.0.get_mut(segment_value)?;
    if slot.is_none() {
        let color = generate_segment_color(state.next_color_index, hint_maximum_segment_type);
        state.next_color_index = state.next_color_index.saturating_add(1);
        *slot = Some(color);
    }
    *slot
}

fn selected_type_quarks(selected_types: Option<&str>) -> Option<Vec<glib::Quark>> {
    let selected = selected_types?;
    let quarks: Vec<_> = selected
        .split(';')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(glib::Quark::from_str)
        .collect();
    if quarks.is_empty() {
        None
    } else {
        Some(quarks)
    }
}

pub(crate) fn related_classification<'a>(
    meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>,
    seg_mtd: &gst_analytics::AnalyticsMtdRef<'a, AnalyticsSegmentationMtd>,
) -> Option<gst_analytics::AnalyticsMtdRef<'a, AnalyticsClassificationMtd>> {
    meta.iter_direct_related::<AnalyticsClassificationMtd>(seg_mtd.id(), RelTypes::N_TO_N)
        .next()
        .or_else(|| {
            meta.iter_direct_related::<AnalyticsClassificationMtd>(
                seg_mtd.id(),
                RelTypes::RELATE_TO,
            )
            .next()
        })
}

fn build_mask_filter(
    cls_mtd: Option<&gst_analytics::AnalyticsMtdRef<'_, AnalyticsClassificationMtd>>,
    selected_types: Option<&[glib::Quark]>,
) -> Option<Vec<bool>> {
    let selected_types = selected_types?;
    let cls_mtd = cls_mtd?;

    let mut filter = vec![false; cls_mtd.len()];
    for (index, allowed) in filter.iter_mut().enumerate() {
        *allowed = selected_types.contains(&cls_mtd.quark(index));
    }
    Some(filter)
}

pub(crate) fn update_selected_type_cache(state: &mut State, selected_types: Option<&str>) {
    let selected_types_owned = selected_types.map(str::to_owned);
    if state.selected_types_source == selected_types_owned {
        return;
    }

    state.selected_types_source = selected_types_owned;
    state.selected_type_quarks = selected_type_quarks(selected_types);
    state.mask_filter_cache = None;
}

pub(crate) fn cached_mask_filter(
    state: &mut State,
    cls_mtd: Option<&gst_analytics::AnalyticsMtdRef<'_, AnalyticsClassificationMtd>>,
    selected_types: Option<&[glib::Quark]>,
) -> Option<Arc<[bool]>> {
    let selected_types = selected_types?;
    let cls_mtd = cls_mtd?;

    let cls_quarks: Vec<_> = (0..cls_mtd.len())
        .map(|index| cls_mtd.quark(index))
        .collect();

    if let Some(cache) = state.mask_filter_cache.as_ref()
        && cache.selected_types.as_deref() == Some(selected_types)
        && cache.cls_quarks == cls_quarks
    {
        return Some(cache.filter.clone());
    }

    let filter_vec = build_mask_filter(Some(cls_mtd), Some(selected_types))?;
    let filter = Arc::<[bool]>::from(filter_vec);
    state.mask_filter_cache = Some(MaskFilterCache {
        selected_types: Some(selected_types.to_vec()),
        cls_quarks,
        filter: filter.clone(),
    });

    Some(filter)
}

fn write_premultiplied_bgra(pixel: &mut [u8], rgb: u32, alpha: u8) {
    let r = ((rgb >> 16) & 0xFF) as u16;
    let g = ((rgb >> 8) & 0xFF) as u16;
    let b = (rgb & 0xFF) as u16;
    let a = alpha as u16;

    let r_p = ((r * a + 127) / 255) as u8;
    let g_p = ((g * a + 127) / 255) as u8;
    let b_p = ((b * a + 127) / 255) as u8;

    pixel[0] = b_p;
    pixel[1] = g_p;
    pixel[2] = r_p;
    pixel[3] = alpha;
}

pub(crate) fn render_mask_canvas(
    mask: gst::Buffer,
    canvas_w: u32,
    canvas_h: u32,
    mask_filter: Option<&[bool]>,
    mut color_for_segment: impl FnMut(usize) -> Option<u32>,
) -> Option<gst::Buffer> {
    if canvas_w == 0 || canvas_h == 0 {
        return None;
    }

    let mask_meta = mask.meta::<gst_video::VideoMeta>()?;
    let mask_info =
        gst_video::VideoInfo::builder(mask_meta.format(), mask_meta.width(), mask_meta.height())
            .build()
            .ok()?;
    let mask_frame = gst_video::VideoFrame::from_buffer_readable(mask, &mask_info).ok()?;
    let mask_data = mask_frame.plane_data(0).ok()?;
    let mask_stride = mask_frame.plane_stride()[0].unsigned_abs() as usize;
    let mask_w = mask_frame.width() as usize;
    let mask_h = mask_frame.height() as usize;
    if mask_w == 0 || mask_h == 0 {
        return None;
    }

    let mut canvas = gst::Buffer::with_size((canvas_w as usize) * (canvas_h as usize) * 4).ok()?;
    gst_video::VideoMeta::add(
        canvas.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Bgra,
        canvas_w,
        canvas_h,
    )
    .ok()?;

    let canvas_info =
        gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgra, canvas_w, canvas_h)
            .build()
            .ok()?;
    let mut canvas_frame =
        gst_video::VideoFrameRef::from_buffer_ref_writable(canvas.make_mut(), &canvas_info).ok()?;
    let canvas_stride = canvas_frame.plane_stride()[0].unsigned_abs() as usize;
    let canvas_data = canvas_frame.plane_data_mut(0).ok()?;

    for y in 0..canvas_h as usize {
        let src_y = (y * mask_h) / canvas_h as usize;
        let src_row = &mask_data[src_y * mask_stride..src_y * mask_stride + mask_w];
        let dst_row =
            &mut canvas_data[y * canvas_stride..y * canvas_stride + (canvas_w as usize * 4)];

        for x in 0..canvas_w as usize {
            let src_x = (x * mask_w) / canvas_w as usize;
            let value = src_row[src_x] as usize;
            let allowed = mask_filter
                .map(|filter| value < filter.len() && filter[value])
                .unwrap_or(true);
            let visible = value != 0 && allowed;

            let dst_px = &mut dst_row[x * 4..x * 4 + 4];
            if visible {
                if let Some(rgb) = color_for_segment(value) {
                    write_premultiplied_bgra(dst_px, rgb, MASK_ALPHA);
                } else {
                    dst_px.copy_from_slice(&[0, 0, 0, 0]);
                }
            } else {
                dst_px.copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    drop(canvas_frame);

    Some(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_to_rgb_produces_distinct_primary_colors() {
        assert_eq!(hue_to_rgb(0.0), 0x00FF_0000);
        assert_eq!(hue_to_rgb(120.0), 0x0000_FF00);
        assert_eq!(hue_to_rgb(240.0), 0x0000_00FF);
    }

    #[test]
    fn selected_type_parser_ignores_empty_tokens() {
        let parsed = selected_type_quarks(Some(" person ; ;car;"));
        let parsed = parsed.unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], glib::Quark::from_str("person"));
        assert_eq!(parsed[1], glib::Quark::from_str("car"));
    }

    #[test]
    fn existing_segment_color_persists_after_hint_change() {
        let mut state = State::default();

        let first = color_for_segment(&mut state, 5, 10).unwrap();
        let _new_color_after_change = color_for_segment(&mut state, 11, 100).unwrap();
        let second = color_for_segment(&mut state, 5, 100).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn update_selected_type_cache_reuses_existing_parse() {
        let mut state = State::default();

        update_selected_type_cache(&mut state, Some("person;car"));
        let first_ptr = state
            .selected_type_quarks
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap();

        update_selected_type_cache(&mut state, Some("person;car"));
        let second_ptr = state
            .selected_type_quarks
            .as_ref()
            .map(|v| v.as_ptr())
            .unwrap();

        assert_eq!(first_ptr, second_ptr);
    }

    #[test]
    fn update_selected_type_cache_invalidates_mask_filter_cache_on_change() {
        let mut state = State {
            mask_filter_cache: Some(MaskFilterCache {
                selected_types: Some(vec![glib::Quark::from_str("person")]),
                cls_quarks: vec![glib::Quark::from_str("person")],
                filter: Arc::<[bool]>::from(vec![true]),
            }),
            ..Default::default()
        };

        update_selected_type_cache(&mut state, Some("car"));

        assert!(state.mask_filter_cache.is_none());
        assert_eq!(
            state.selected_type_quarks,
            Some(vec![glib::Quark::from_str("car")])
        );
    }
}
