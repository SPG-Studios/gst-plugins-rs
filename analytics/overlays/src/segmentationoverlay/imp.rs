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
use gst_analytics::{
    AnalyticsClassificationMtd, AnalyticsMetaRefExt, AnalyticsMtd, AnalyticsRelationMeta, RelTypes,
};

use glib::translate::{UnsafeFrom, from_glib_full};

use gst_base::prelude::BaseTransformExt;
use gst_base::subclass::prelude::*;
use gst_video::prelude::VideoFrameExt;
use gst_video::subclass::prelude::*;

use crate::lifecycle::{OverlayLifecycle, lifecycle_event_kind};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{LazyLock, Mutex};

const DEFAULT_RENDER_ENABLED: bool = false;
const DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE: u32 = 10;
const MASK_ALPHA: u8 = 0x80;

#[derive(Debug, Clone)]
struct Settings {
    render_enabled: bool,
    hint_maximum_segment_type: u32,
    selected_types: Option<String>,
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

#[derive(Default)]
pub struct SegmentationOverlay {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    segment_colors: HashMap<usize, u32>,
    next_color_index: u64,
    composition: Option<gst_video::VideoOverlayComposition>,
    attach_composition: bool,
    selected_types_source: Option<String>,
    selected_type_quarks: Option<Vec<glib::Quark>>,
    mask_filter_cache: Option<MaskFilterCache>,
}

struct MaskFilterCache {
    selected_types: Option<Vec<glib::Quark>>,
    cls_quarks: Vec<glib::Quark>,
    filter: Arc<[bool]>,
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "segoverlay",
        gst::DebugColorFlags::empty(),
        Some("Segmentation overlay"),
    )
});

static BLEND_CAPS: LazyLock<gst::Caps> =
    LazyLock::new(|| gst_video::VideoCapsBuilder::new().build());

#[derive(Debug)]
enum AnalyticsSegmentationMtd {}

unsafe impl AnalyticsMtd for AnalyticsSegmentationMtd {
    fn mtd_type() -> gst_analytics::ffi::GstAnalyticsMtdType {
        unsafe { gst_analytics::ffi::gst_analytics_segmentation_mtd_get_mtd_type() }
    }
}

trait SegmentationMtdExt {
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

fn color_for_segment(
    state: &mut State,
    segment_value: usize,
    hint_maximum_segment_type: u32,
) -> Option<u32> {
    if segment_value == 0 {
        return None;
    }

    let entry = state
        .segment_colors
        .entry(segment_value)
        .or_insert_with(|| {
            let color = generate_segment_color(state.next_color_index, hint_maximum_segment_type);
            state.next_color_index = state.next_color_index.saturating_add(1);
            color
        });

    Some(*entry)
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

fn related_classification<'a>(
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

fn update_selected_type_cache(state: &mut State, selected_types: Option<&str>) {
    let selected_types_owned = selected_types.map(str::to_owned);
    if state.selected_types_source == selected_types_owned {
        return;
    }

    state.selected_types_source = selected_types_owned;
    state.selected_type_quarks = selected_type_quarks(selected_types);
    state.mask_filter_cache = None;
}

fn cached_mask_filter(
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

fn render_mask_canvas(
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

fn can_handle_caps(in_caps: &gst::Caps) -> bool {
    in_caps.is_subset(&BLEND_CAPS)
}

fn decide_attach_mode(
    upstream_has_meta: bool,
    caps_has_meta: bool,
    alloc_has_meta: bool,
    can_handle: bool,
) -> Result<bool, ()> {
    let attach = if upstream_has_meta {
        true
    } else if caps_has_meta {
        if alloc_has_meta { true } else { !can_handle }
    } else {
        false
    };

    if !attach && !can_handle {
        Err(())
    } else {
        Ok(attach)
    }
}

impl SegmentationOverlay {
    fn reset_runtime_state(&self) {
        let mut state = self.state.lock().unwrap();
        state.segment_colors.clear();
        state.next_color_index = 0;
        state.composition = None;
        state.attach_composition = false;
        state.mask_filter_cache = None;
    }

    fn negotiate_attach_mode(&self, in_caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let srcpad = self.obj().upcast_ref::<gst::Element>().static_pad("src");
        let Some(srcpad) = srcpad else {
            self.state.lock().unwrap().attach_composition = false;
            return Ok(());
        };

        let upstream_has_meta = in_caps
            .features(0)
            .map(|f| f.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION))
            .unwrap_or(false);

        let mut caps = in_caps.clone();
        let mut caps_has_meta = false;
        let mut alloc_has_meta = false;

        if !upstream_has_meta {
            let mut caps_clone = caps.clone();
            if let Some(features) = caps_clone.make_mut().features_mut(0) {
                let is_sysmem = features.is_empty()
                    || features
                        .iter()
                        .all(|feature| feature == gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY);

                features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                let peercaps = srcpad.peer_query_caps(Some(&caps_clone));
                caps_has_meta = !peercaps.is_empty();
                if caps_has_meta {
                    caps = caps_clone;
                } else if !is_sysmem {
                    return Err(gst::loggable_error!(
                        CAT,
                        "Provided input caps with features but downstream does not support meta::GstVideoOverlayComposition"
                    ));
                }
            }
        }

        if upstream_has_meta || caps_has_meta {
            let mut query = gst::query::Allocation::new(Some(&caps), false);
            if srcpad.peer_query(&mut query) {
                alloc_has_meta = query
                    .find_allocation_meta::<gst_video::VideoOverlayCompositionMeta>()
                    .is_some();
            } else if srcpad.pad_flags().contains(gst::PadFlags::FLUSHING) {
                srcpad.mark_reconfigure();
                return Err(gst::loggable_error!(
                    CAT,
                    "ALLOCATION query failed while pad is flushing"
                ));
            }
        }

        let can_handle = can_handle_caps(in_caps);
        let attach = match decide_attach_mode(
            upstream_has_meta,
            caps_has_meta,
            alloc_has_meta,
            can_handle,
        ) {
            Ok(attach) => attach,
            Err(()) => {
                srcpad.mark_reconfigure();
                return Err(gst::loggable_error!(
                    CAT,
                    "Unsupported caps for blending: {}",
                    in_caps
                ));
            }
        };

        gst::debug!(
            CAT,
            imp = self,
            "attach mode negotiated: upstream_has_meta={}, caps_has_meta={}, alloc_has_meta={}, can_handle={}, attach={}",
            upstream_has_meta,
            caps_has_meta,
            alloc_has_meta,
            can_handle,
            attach,
        );

        self.state.lock().unwrap().attach_composition = attach;
        Ok(())
    }
}

impl OverlayLifecycle for SegmentationOverlay {
    fn reset_runtime_state(&self) {
        SegmentationOverlay::reset_runtime_state(self);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SegmentationOverlay {
    const NAME: &'static str = "GstSegmentationOverlay";
    type Type = super::SegmentationOverlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for SegmentationOverlay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("render-enabled")
                    .nick("Render enabled")
                    .blurb("When false, element runs in passthrough mode")
                    .default_value(DEFAULT_RENDER_ENABLED)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("hint-maximum-segment-type")
                    .nick("Hint maximum segment type")
                    .blurb("Hint for expected maximum segment type value")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_HINT_MAXIMUM_SEGMENT_TYPE)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("selected-types")
                    .nick("Selected types")
                    .blurb("Semicolon-separated type names to render")
                    .default_value(None)
                    .mutable_playing()
                    .build(),
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
            "hint-maximum-segment-type" => {
                let mut settings = self.settings.lock().unwrap();
                settings.hint_maximum_segment_type = value.get().expect("type checked upstream");
            }
            "selected-types" => {
                let mut settings = self.settings.lock().unwrap();
                settings.selected_types = value.get().expect("type checked upstream");

                let mut state = self.state.lock().unwrap();
                update_selected_type_cache(&mut state, settings.selected_types.as_deref());
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
            "hint-maximum-segment-type" => {
                let settings = self.settings.lock().unwrap();
                settings.hint_maximum_segment_type.to_value()
            }
            "selected-types" => {
                let settings = self.settings.lock().unwrap();
                settings.selected_types.to_value()
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

impl GstObjectImpl for SegmentationOverlay {}

impl ElementImpl for SegmentationOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Segmentation Overlay",
                "Filter/Editor/Video",
                "Overlay a visual representation of segmentation metadata on the video",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = BLEND_CAPS.clone();

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

impl BaseTransformImpl for SegmentationOverlay {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.lifecycle_start()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        self.lifecycle_stop()
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        match lifecycle_event_kind(&event) {
            Some(_) => {
                self.reset_runtime_state();
                self.parent_sink_event(event)
            }
            None => self.parent_sink_event(event),
        }
    }
}

impl VideoFilterImpl for SegmentationOverlay {
    fn set_info(
        &self,
        in_caps: &gst::Caps,
        _in_info: &gst_video::VideoInfo,
        _out_caps: &gst::Caps,
        _out_info: &gst_video::VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        self.negotiate_attach_mode(in_caps)
    }

    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();

        update_selected_type_cache(&mut state, settings.selected_types.as_deref());

        let selected_types = state.selected_type_quarks.clone();

        if let Some(meta) = frame.buffer().meta::<AnalyticsRelationMeta>() {
            let mut composition = frame
                .buffer()
                .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
                .next()
                .map(|overlay_meta| overlay_meta.overlay().to_owned());

            for seg_mtd in meta.iter::<AnalyticsSegmentationMtd>() {
                let Some((mask, mut ofx, mut ofy, mut canvas_w, mut canvas_h)) = seg_mtd.mask()
                else {
                    continue;
                };

                if canvas_w == 0 || canvas_h == 0 {
                    continue;
                }

                let frame_w = frame.width() as i32;
                let frame_h = frame.height() as i32;
                ofx = ofx.clamp(0, frame_w);
                ofy = ofy.clamp(0, frame_h);
                canvas_w = canvas_w.min((frame_w - ofx).max(0) as u32);
                canvas_h = canvas_h.min((frame_h - ofy).max(0) as u32);

                if canvas_w == 0 || canvas_h == 0 {
                    continue;
                }

                let cls_mtd = related_classification(&meta, &seg_mtd);
                let mask_filter =
                    cached_mask_filter(&mut state, cls_mtd.as_ref(), selected_types.as_deref());
                let Some(canvas) = render_mask_canvas(
                    mask,
                    canvas_w,
                    canvas_h,
                    mask_filter.as_deref(),
                    |segment_value| {
                        color_for_segment(
                            &mut state,
                            segment_value,
                            settings.hint_maximum_segment_type,
                        )
                    },
                ) else {
                    continue;
                };

                let rect = gst_video::VideoOverlayRectangle::new_raw(
                    &canvas,
                    ofx,
                    ofy,
                    canvas_w,
                    canvas_h,
                    gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
                );

                if let Some(composition_ref) = composition.as_mut() {
                    if let Some(composition_mut) = composition_ref.get_mut() {
                        composition_mut.add_rectangle(&rect);
                    }
                } else {
                    composition = gst_video::VideoOverlayComposition::new(Some(&rect)).ok();
                }
            }

            state.composition = composition;
        }

        let composition = state.composition.clone();
        let attach = state.attach_composition;
        drop(state);

        if let Some(composition) = composition {
            if attach {
                // SAFETY: The frame is writable and uniquely borrowed here.
                let buffer = unsafe { gst::BufferRef::from_mut_ptr((*frame.as_mut_ptr()).buffer) };
                gst_video::VideoOverlayCompositionMeta::add(buffer, &composition);
            } else {
                composition
                    .blend(frame)
                    .map_err(|_| gst::FlowError::Error)?;
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
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
    fn can_handle_caps_accepts_video_raw_caps() {
        let caps = gst_video::VideoCapsBuilder::new()
            .format(gst_video::VideoFormat::I420)
            .build();
        assert!(can_handle_caps(&caps));
    }

    #[test]
    fn can_handle_caps_rejects_non_video_caps() {
        let caps = gst::Caps::builder("audio/x-raw").build();
        assert!(!can_handle_caps(&caps));
    }

    #[test]
    fn reset_runtime_state_clears_cached_overlay_state() {
        let overlay = SegmentationOverlay::default();

        {
            let mut state = overlay.state.lock().unwrap();
            state.segment_colors.insert(1, 0x00ff_0000);
            state.next_color_index = 1;

            let mut overlay_buf = gst::Buffer::from_mut_slice(vec![0_u8; 4]);
            gst_video::VideoMeta::add(
                overlay_buf.get_mut().unwrap(),
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Bgra,
                1,
                1,
            )
            .unwrap();

            let rect = gst_video::VideoOverlayRectangle::new_raw(
                &overlay_buf,
                0,
                0,
                1,
                1,
                gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
            );
            state.composition = gst_video::VideoOverlayComposition::new(Some(&rect)).ok();
            state.attach_composition = true;
        }

        overlay.reset_runtime_state();

        let state = overlay.state.lock().unwrap();
        assert!(state.segment_colors.is_empty());
        assert_eq!(state.next_color_index, 0);
        assert!(state.composition.is_none());
        assert!(!state.attach_composition);
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
    fn decide_attach_mode_upstream_meta_always_attaches() {
        let attach = decide_attach_mode(true, false, false, true).unwrap();
        assert!(attach);
    }

    #[test]
    fn decide_attach_mode_caps_and_alloc_meta_attaches() {
        let attach = decide_attach_mode(false, true, true, true).unwrap();
        assert!(attach);
    }

    #[test]
    fn decide_attach_mode_caps_meta_without_alloc_prefers_blend_if_possible() {
        let attach = decide_attach_mode(false, true, false, true).unwrap();
        assert!(!attach);
    }

    #[test]
    fn decide_attach_mode_caps_meta_without_alloc_forces_attach_when_cannot_blend() {
        let attach = decide_attach_mode(false, true, false, false).unwrap();
        assert!(attach);
    }

    #[test]
    fn decide_attach_mode_without_meta_and_without_blend_support_fails() {
        let result = decide_attach_mode(false, false, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn decide_attach_mode_without_meta_but_with_blend_support_blends() {
        let attach = decide_attach_mode(false, false, false, true).unwrap();
        assert!(!attach);
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
