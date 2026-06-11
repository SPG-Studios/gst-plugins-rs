// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#![allow(dead_code)]

use gst::BufferRef;
use gst_video::prelude::VideoFrameExt;
use gst_video::{VideoFormat, VideoFrameRef};

use crate::geometry::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RenderBackendKind {
    #[default]
    Skia,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AnalyticsFrame<'a> {
    pub object_count: usize,
    pub segment_count: usize,
    pub keypoint_count: usize,
    pub semantic_tag: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DrawCommand {
    NoOp,
    Rectangle {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        rotation: f32,
        argb: u32,
        filled: bool,
    },
    Circle {
        cx: f32,
        cy: f32,
        radius: f32,
        argb: u32,
    },
    Line {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        argb: u32,
        width: f32,
    },
    Text {
        x: f32,
        y: f32,
        text: String,
        argb: u32,
    },
    TextCentered {
        x: f32,
        y: f32,
        text: String,
        argb: u32,
    },
}

trait RenderBackend {
    fn render_frame(
        &mut self,
        frame: &mut VideoFrameRef<&mut BufferRef>,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Result<(), gst::FlowError>;
}

#[derive(Debug, Clone, Copy)]
struct PackedPixelLayout {
    alpha: Option<usize>,
    red: usize,
    green: usize,
    blue: usize,
}

struct PackedSurface<'a> {
    data: &'a mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    layout: PackedPixelLayout,
}

const LABEL_FONT_SIZE: f32 = 10.5;
const LABEL_STROKE_WIDTH: f32 = 0.6;
const KEYPOINT_LABEL_STROKE_WIDTH: f32 = 1.0;
const BOX_STROKE_WIDTH: f32 = 2.0;
const LABEL_EXTRA_VERTICAL_GAP: f32 = 1.5;
const ROTATION_EPSILON: f32 = 0.001;
pub(crate) const LABEL_LAYOUT_HEIGHT: i32 = 12;
pub(crate) const LABEL_LAYOUT_GAP: i32 = 2;
/// Width of the leader line drawn from a feature to a displaced label.
pub(crate) const LEADER_LINE_WIDTH: f32 = 1.0;

fn label_outline_offset(font_size: f32) -> f32 {
    (font_size / 15.0).max(1.0)
}

fn make_label_font() -> skia::Font {
    let font_mgr = skia::FontMgr::default();
    let typeface = ["Arial", "Liberation Sans", "DejaVu Sans", "Sans"]
        .iter()
        .find_map(|family| font_mgr.match_family_style(*family, skia::FontStyle::normal()))
        .or_else(|| font_mgr.legacy_make_typeface(None, skia::FontStyle::normal()));

    let mut font = if let Some(typeface) = typeface {
        skia::Font::from_typeface(typeface, LABEL_FONT_SIZE)
    } else {
        let mut default_font = skia::Font::default();
        default_font.set_size(LABEL_FONT_SIZE);
        default_font
    };
    font.set_subpixel(true);
    font.set_edging(skia::font::Edging::AntiAlias);
    font.set_linear_metrics(true);
    font.set_hinting(skia::FontHinting::Normal);
    font
}

fn measure_text_width_with_stroke(text: &str, stroke_width: f32) -> i32 {
    let font = make_label_font();
    let mut paint = skia::Paint::default();
    paint.set_style(skia::paint::Style::Stroke);
    paint.set_stroke_width(stroke_width);

    let (_, bounds) = font.measure_str(text, Some(&paint));
    let outline = label_outline_offset(LABEL_FONT_SIZE);
    (bounds.width() + outline * 2.0).ceil().max(1.0) as i32
}

pub(crate) fn measure_label_text_width(text: &str) -> i32 {
    measure_text_width_with_stroke(text, LABEL_STROKE_WIDTH)
}

pub(crate) fn measure_centered_label_text_width(text: &str) -> i32 {
    measure_text_width_with_stroke(text, KEYPOINT_LABEL_STROKE_WIDTH)
}

/// Bounding box of a command's visible, "solid" content, used to publish claimed
/// regions for cross-element coordination. Thin strokes (skeleton and leader
/// lines) and no-ops return `None`. Box rotation is ignored — the axis-aligned
/// extent is claimed.
pub(crate) fn content_bounds(command: &DrawCommand) -> Option<Rect> {
    match command {
        DrawCommand::Rectangle {
            x,
            y,
            width,
            height,
            ..
        } => Some(Rect::from_xywh(
            x.floor() as i32,
            y.floor() as i32,
            width.ceil() as i32,
            height.ceil() as i32,
        )),
        DrawCommand::Circle { cx, cy, radius, .. } => {
            let r = radius.ceil() as i32;
            Some(Rect::from_xywh(
                (*cx as i32).saturating_sub(r),
                (*cy as i32).saturating_sub(r),
                r.saturating_mul(2).saturating_add(1),
                r.saturating_mul(2).saturating_add(1),
            ))
        }
        // Left-aligned label, anchored at its bottom-left (see `push_od_label`).
        DrawCommand::Text { x, y, text, .. } => {
            let w = measure_label_text_width(text);
            Some(Rect::from_xywh(
                *x as i32,
                (*y as i32) - LABEL_LAYOUT_HEIGHT,
                w,
                LABEL_LAYOUT_HEIGHT,
            ))
        }
        // Centered label, anchored at its center (see `push_keypoint_label`).
        DrawCommand::TextCentered { x, y, text, .. } => {
            let w = measure_centered_label_text_width(text);
            Some(Rect::from_xywh(
                (*x as i32) - w / 2,
                (*y as i32) - LABEL_LAYOUT_HEIGHT / 2,
                w,
                LABEL_LAYOUT_HEIGHT,
            ))
        }
        DrawCommand::Line { .. } | DrawCommand::NoOp => None,
    }
}

fn packed_pixel_layout(format: VideoFormat) -> Option<PackedPixelLayout> {
    match format {
        VideoFormat::Bgra | VideoFormat::Bgrx => Some(PackedPixelLayout {
            alpha: matches!(format, VideoFormat::Bgra).then_some(3),
            red: 2,
            green: 1,
            blue: 0,
        }),
        VideoFormat::Rgba | VideoFormat::Rgbx => Some(PackedPixelLayout {
            alpha: matches!(format, VideoFormat::Rgba).then_some(3),
            red: 0,
            green: 1,
            blue: 2,
        }),
        VideoFormat::Argb | VideoFormat::Xrgb => Some(PackedPixelLayout {
            alpha: matches!(format, VideoFormat::Argb).then_some(0),
            red: 1,
            green: 2,
            blue: 3,
        }),
        VideoFormat::Abgr | VideoFormat::Xbgr => Some(PackedPixelLayout {
            alpha: matches!(format, VideoFormat::Abgr).then_some(0),
            red: 3,
            green: 2,
            blue: 1,
        }),
        _ => None,
    }
}

fn gst_to_skia(video_format: VideoFormat) -> Option<skia::ColorType> {
    match video_format {
        VideoFormat::Rgba => Some(skia::ColorType::RGBA8888),
        VideoFormat::Rgbx => Some(skia::ColorType::RGB888x),
        VideoFormat::Bgra | VideoFormat::Bgrx => Some(skia::ColorType::BGRA8888),
        _ => None,
    }
}

fn argb_to_skia_color(argb: u32) -> skia::Color {
    let alpha = ((argb >> 24) & 0xFF) as u8;
    let red = ((argb >> 16) & 0xFF) as u8;
    let green = ((argb >> 8) & 0xFF) as u8;
    let blue = (argb & 0xFF) as u8;
    skia::Color::from_argb(alpha, red, green, blue)
}

fn write_packed_pixel(pixel: &mut [u8], layout: PackedPixelLayout, argb: u32) {
    let alpha = ((argb >> 24) & 0xFF) as u8;
    let red = ((argb >> 16) & 0xFF) as u8;
    let green = ((argb >> 8) & 0xFF) as u8;
    let blue = (argb & 0xFF) as u8;

    if let Some(alpha_index) = layout.alpha {
        pixel[alpha_index] = alpha;
    }

    pixel[layout.red] = red;
    pixel[layout.green] = green;
    pixel[layout.blue] = blue;
}

fn draw_packed_rectangle(
    surface: &mut PackedSurface<'_>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    argb: u32,
    filled: bool,
) {
    let left = x.floor().max(0.0) as usize;
    let top = y.floor().max(0.0) as usize;
    let right = (x + width).ceil().max(0.0) as usize;
    let bottom = (y + height).ceil().max(0.0) as usize;

    let clamped_left = left.min(surface.width);
    let clamped_top = top.min(surface.height);
    let clamped_right = right.min(surface.width);
    let clamped_bottom = bottom.min(surface.height);

    if clamped_left >= clamped_right || clamped_top >= clamped_bottom {
        return;
    }

    for row in clamped_top..clamped_bottom {
        let row_start = row * surface.stride;

        for col in clamped_left..clamped_right {
            if !filled
                && row != clamped_top
                && row + 1 != clamped_bottom
                && col != clamped_left
                && col + 1 != clamped_right
            {
                continue;
            }

            let pixel_offset = row_start + col * 4;
            let pixel = &mut surface.data[pixel_offset..pixel_offset + 4];
            write_packed_pixel(pixel, surface.layout, argb);
        }
    }
}

fn draw_packed_circle(surface: &mut PackedSurface<'_>, cx: f32, cy: f32, radius: f32, argb: u32) {
    let radius = radius.max(0.5);
    let left = (cx - radius).floor().max(0.0) as i32;
    let top = (cy - radius).floor().max(0.0) as i32;
    let right = (cx + radius)
        .ceil()
        .min(surface.width.saturating_sub(1) as f32)
        .max(0.0) as i32;
    let bottom = (cy + radius)
        .ceil()
        .min(surface.height.saturating_sub(1) as f32)
        .max(0.0) as i32;

    if left > right || top > bottom {
        return;
    }

    for y in top..=bottom {
        for x in left..=right {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= radius * radius {
                set_surface_pixel(surface, x, y, argb);
            }
        }
    }
}

fn draw_packed_line(
    surface: &mut PackedSurface<'_>,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    argb: u32,
    width: f32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).ceil().max(1.0) as usize;
    let radius = (width.max(1.0) / 2.0).max(0.5);

    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        draw_packed_circle(surface, x0 + dx * t, y0 + dy * t, radius, argb);
    }
}

fn set_surface_pixel(surface: &mut PackedSurface<'_>, x: i32, y: i32, argb: u32) {
    if x < 0 || y < 0 {
        return;
    }

    let x = x as usize;
    let y = y as usize;

    if x >= surface.width || y >= surface.height {
        return;
    }

    let pixel_offset = y * surface.stride + x * 4;
    let pixel = &mut surface.data[pixel_offset..pixel_offset + 4];
    write_packed_pixel(pixel, surface.layout, argb);
}

fn draw_packed_text(surface: &mut PackedSurface<'_>, x: f32, y: f32, text: &str, argb: u32) {
    // Fallback renderer for formats unsupported by Skia path.
    let mut pen_x = x.floor() as i32;
    let pen_y = y.floor() as i32;
    for _ in text.chars().take(64) {
        for row in 0..10 {
            for col in 0..6 {
                if row == 0 || row == 9 || col == 0 || col == 5 {
                    set_surface_pixel(surface, pen_x + col, pen_y + row, argb);
                }
            }
        }
        pen_x += 8;
    }
}

fn draw_packed_text_centered(
    surface: &mut PackedSurface<'_>,
    x: f32,
    y: f32,
    text: &str,
    argb: u32,
) {
    // The fallback glyph is 10px tall, so shift by 5px to center around y.
    draw_packed_text(surface, x, y - 5.0, text, argb);
}

#[derive(Debug, Default)]
struct SkiaBackend;

impl RenderBackend for SkiaBackend {
    fn render_frame(
        &mut self,
        frame: &mut VideoFrameRef<&mut BufferRef>,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Result<(), gst::FlowError> {
        let _ = analytics;

        if let Some(color_type) = gst_to_skia(frame.format()) {
            let width = frame.width() as i32;
            let height = frame.height() as i32;
            let img_info = skia::ImageInfo::new(
                skia::ISize { width, height },
                color_type,
                skia::AlphaType::Unpremul,
                None,
            );

            let stride = frame.plane_stride()[0].unsigned_abs() as usize;
            let data = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;
            let mut surface = skia::surface::surfaces::wrap_pixels(&img_info, data, stride, None)
                .ok_or(gst::FlowError::Error)?;

            let canvas = surface.canvas();
            let font = make_label_font();

            let outline_ofs = label_outline_offset(LABEL_FONT_SIZE);

            for command in commands {
                match command {
                    DrawCommand::Rectangle {
                        x,
                        y,
                        width,
                        height,
                        rotation,
                        argb,
                        filled,
                    } => {
                        let mut paint = skia::Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_color(argb_to_skia_color(*argb));
                        if *filled {
                            paint.set_style(skia::paint::Style::Fill);
                        } else {
                            paint.set_style(skia::paint::Style::Stroke);
                            paint.set_stroke_width(BOX_STROKE_WIDTH);
                        }

                        if rotation.abs() < ROTATION_EPSILON {
                            let rect = skia::Rect::from_xywh(*x, *y, *width, *height);
                            canvas.draw_rect(rect, &paint);
                        } else {
                            let xc = *x + *width / 2.0;
                            let yc = *y + *height / 2.0;
                            let cos_r = rotation.cos();
                            let sin_r = rotation.sin();

                            let corners = [
                                (-*width / 2.0, -*height / 2.0),
                                (*width / 2.0, -*height / 2.0),
                                (*width / 2.0, *height / 2.0),
                                (-*width / 2.0, *height / 2.0),
                            ];

                            let mut path_builder = skia::PathBuilder::new();
                            for (index, (dx, dy)) in corners.iter().copied().enumerate() {
                                let rx = dx * cos_r - dy * sin_r + xc;
                                let ry = dx * sin_r + dy * cos_r + yc;

                                if index == 0 {
                                    path_builder.move_to((rx, ry));
                                } else {
                                    path_builder.line_to((rx, ry));
                                }
                            }
                            path_builder.close();
                            let path = path_builder.detach();
                            canvas.draw_path(&path, &paint);
                        }
                    }
                    DrawCommand::Circle {
                        cx,
                        cy,
                        radius,
                        argb,
                    } => {
                        let mut paint = skia::Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_color(argb_to_skia_color(*argb));
                        paint.set_style(skia::paint::Style::Fill);
                        canvas.draw_circle(skia::Point::new(*cx, *cy), *radius, &paint);
                    }
                    DrawCommand::Line {
                        x0,
                        y0,
                        x1,
                        y1,
                        argb,
                        width,
                    } => {
                        let mut paint = skia::Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_color(argb_to_skia_color(*argb));
                        paint.set_style(skia::paint::Style::Stroke);
                        paint.set_stroke_width(*width);
                        canvas.draw_line(
                            skia::Point::new(*x0, *y0),
                            skia::Point::new(*x1, *y1),
                            &paint,
                        );
                    }
                    DrawCommand::Text { x, y, text, argb } => {
                        let mut paint = skia::Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_color(argb_to_skia_color(*argb));
                        paint.set_style(skia::paint::Style::Stroke);
                        paint.set_stroke_width(LABEL_STROKE_WIDTH);

                        // Place text so its bottom sits just above the provided anchor y.
                        let (_, bounds) = font.measure_str(text, Some(&paint));
                        let draw_x = *x + outline_ofs;
                        let baseline_y =
                            *y - outline_ofs - LABEL_EXTRA_VERTICAL_GAP - bounds.bottom();

                        canvas.draw_str(text, (draw_x, baseline_y), &font, &paint);
                    }
                    DrawCommand::TextCentered { x, y, text, argb } => {
                        let mut paint = skia::Paint::default();
                        paint.set_anti_alias(true);
                        paint.set_color(argb_to_skia_color(*argb));
                        paint.set_style(skia::paint::Style::Stroke);
                        paint.set_stroke_width(KEYPOINT_LABEL_STROKE_WIDTH);

                        let (_, bounds) = font.measure_str(text, Some(&paint));
                        let draw_x = *x + outline_ofs;
                        let baseline_y = *y - (bounds.top() + bounds.bottom()) / 2.0;

                        canvas.draw_str(text, (draw_x, baseline_y), &font, &paint);
                    }
                    DrawCommand::NoOp => {}
                }
            }

            return Ok(());
        }

        let Some(layout) = packed_pixel_layout(frame.format()) else {
            return Ok(());
        };

        let width = frame.width() as usize;
        let height = frame.height() as usize;
        let stride = frame.plane_stride()[0].unsigned_abs() as usize;
        let data = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;
        let mut surface = PackedSurface {
            data,
            stride,
            width,
            height,
            layout,
        };

        for command in commands {
            match command {
                DrawCommand::Rectangle {
                    x,
                    y,
                    width,
                    height,
                    argb,
                    filled,
                    ..
                } => draw_packed_rectangle(&mut surface, *x, *y, *width, *height, *argb, *filled),
                DrawCommand::Circle {
                    cx,
                    cy,
                    radius,
                    argb,
                } => draw_packed_circle(&mut surface, *cx, *cy, *radius, *argb),
                DrawCommand::Line {
                    x0,
                    y0,
                    x1,
                    y1,
                    argb,
                    width,
                } => draw_packed_line(&mut surface, *x0, *y0, *x1, *y1, *argb, *width),
                DrawCommand::Text { x, y, text, argb } => {
                    draw_packed_text(&mut surface, *x, *y, text, *argb)
                }
                DrawCommand::TextCentered { x, y, text, argb } => {
                    draw_packed_text_centered(&mut surface, *x, *y, text, *argb)
                }
                DrawCommand::NoOp => {}
            }
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
pub(crate) struct RenderContext {
    backend_kind: RenderBackendKind,
    skia: SkiaBackend,
}

impl RenderContext {
    pub(crate) fn new(backend_kind: RenderBackendKind) -> Self {
        Self {
            backend_kind,
            skia: SkiaBackend,
        }
    }

    pub(crate) fn backend_kind(&self) -> RenderBackendKind {
        self.backend_kind
    }

    pub(crate) fn render(
        &mut self,
        frame: &mut VideoFrameRef<&mut BufferRef>,
        analytics: &AnalyticsFrame,
        commands: &[DrawCommand],
    ) -> Result<(), gst::FlowError> {
        match self.backend_kind {
            RenderBackendKind::Skia => self.skia.render_frame(frame, analytics, commands),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel_at(data: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        let offset = y * stride + x * 4;
        data[offset..offset + 4].try_into().unwrap()
    }

    #[test]
    fn render_context_defaults_to_skia_backend() {
        let context = RenderContext::default();
        assert_eq!(context.backend_kind(), RenderBackendKind::Skia);
    }

    #[test]
    fn content_bounds_covers_solid_commands_and_skips_strokes() {
        // Rectangle: axis-aligned extent.
        assert_eq!(
            content_bounds(&DrawCommand::Rectangle {
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 30.0,
                rotation: 0.0,
                argb: 0,
                filled: false,
            }),
            Some(Rect::from_xywh(10, 20, 40, 30))
        );

        // Circle: bounding box around the centre.
        assert_eq!(
            content_bounds(&DrawCommand::Circle {
                cx: 50.0,
                cy: 60.0,
                radius: 3.0,
                argb: 0,
            }),
            Some(Rect::from_xywh(47, 57, 7, 7))
        );

        // Left-aligned label: anchored bottom-left, so the rect sits above y.
        let text = DrawCommand::Text {
            x: 12.0,
            y: 24.0,
            text: "hi".to_string(),
            argb: 0,
        };
        let bounds = content_bounds(&text).expect("text has bounds");
        assert_eq!((bounds.left, bounds.bottom), (12, 24));
        assert_eq!(bounds.height(), LABEL_LAYOUT_HEIGHT);
        assert!(bounds.width() > 0);

        // Thin strokes and no-ops are not claimed.
        assert_eq!(
            content_bounds(&DrawCommand::Line {
                x0: 0.0,
                y0: 0.0,
                x1: 9.0,
                y1: 9.0,
                argb: 0,
                width: 1.0,
            }),
            None
        );
        assert_eq!(content_bounds(&DrawCommand::NoOp), None);
    }

    #[test]
    fn analytics_frame_defaults_to_empty() {
        let frame = AnalyticsFrame::default();

        assert_eq!(frame.object_count, 0);
        assert_eq!(frame.segment_count, 0);
        assert_eq!(frame.keypoint_count, 0);
        assert_eq!(frame.semantic_tag, None);
    }

    #[test]
    fn draw_command_variants_are_constructible() {
        let commands = [
            DrawCommand::NoOp,
            DrawCommand::Rectangle {
                x: 1.0,
                y: 2.0,
                width: 10.0,
                height: 20.0,
                rotation: 0.0,
                argb: 0xFFFF_FFFF,
                filled: false,
            },
            DrawCommand::Circle {
                cx: 3.0,
                cy: 4.0,
                radius: 5.0,
                argb: 0xFFFF_0000,
            },
            DrawCommand::Line {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
                argb: 0xFF00_FF00,
                width: 2.0,
            },
            DrawCommand::Text {
                x: 8.0,
                y: 9.0,
                text: "hello".to_string(),
                argb: 0xFFFF_FFFF,
            },
        ];

        assert_eq!(commands.len(), 5);
    }

    #[test]
    fn measured_label_text_width_is_positive() {
        assert!(measure_label_text_width("person (c=0.85)") > 0);
        assert!(measure_centered_label_text_width("0.90") > 0);
    }

    #[test]
    fn packed_rectangle_draws_outline_on_bgra_buffer() {
        let mut pixels = vec![0_u8; 4 * 4 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 16,
            width: 4,
            height: 4,
            layout: packed_pixel_layout(VideoFormat::Bgra).unwrap(),
        };

        draw_packed_rectangle(&mut surface, 1.0, 1.0, 2.0, 2.0, 0xFF11_2233, false);

        assert_eq!(pixel_at(&pixels, 16, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel_at(&pixels, 16, 1, 1), [0x33, 0x22, 0x11, 0xFF]);
        assert_eq!(pixel_at(&pixels, 16, 2, 1), [0x33, 0x22, 0x11, 0xFF]);
        assert_eq!(pixel_at(&pixels, 16, 1, 2), [0x33, 0x22, 0x11, 0xFF]);
        assert_eq!(pixel_at(&pixels, 16, 2, 2), [0x33, 0x22, 0x11, 0xFF]);
        assert_eq!(pixel_at(&pixels, 16, 3, 3), [0, 0, 0, 0]);
    }

    #[test]
    fn packed_filled_rectangle_clips_to_frame_bounds() {
        let mut pixels = vec![0_u8; 3 * 3 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 12,
            width: 3,
            height: 3,
            layout: packed_pixel_layout(VideoFormat::Rgba).unwrap(),
        };

        draw_packed_rectangle(&mut surface, -1.0, -1.0, 3.0, 3.0, 0x8044_5566, true);

        assert_eq!(pixel_at(&pixels, 12, 0, 0), [0x44, 0x55, 0x66, 0x80]);
        assert_eq!(pixel_at(&pixels, 12, 1, 1), [0x44, 0x55, 0x66, 0x80]);
        assert_eq!(pixel_at(&pixels, 12, 2, 2), [0, 0, 0, 0]);
    }

    #[test]
    fn packed_text_draws_pixels_on_bgra_buffer() {
        let mut pixels = vec![0_u8; 32 * 16 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 32 * 4,
            width: 32,
            height: 16,
            layout: packed_pixel_layout(VideoFormat::Bgra).unwrap(),
        };

        draw_packed_text(&mut surface, 2.0, 2.0, "0.8", 0xFFAA_BBCC);

        assert!(pixels.chunks_exact(4).any(|px| px != [0, 0, 0, 0]));
    }

    #[test]
    fn packed_text_clips_without_panicking() {
        let mut pixels = vec![0_u8; 8 * 8 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 8 * 4,
            width: 8,
            height: 8,
            layout: packed_pixel_layout(VideoFormat::Rgba).unwrap(),
        };

        draw_packed_text(&mut surface, -4.0, -3.0, "99", 0xFF11_2233);

        assert!(pixels.chunks_exact(4).any(|px| px != [0, 0, 0, 0]));
    }

    #[test]
    fn packed_text_centered_draws_pixels_on_bgra_buffer() {
        let mut pixels = vec![0_u8; 32 * 16 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 32 * 4,
            width: 32,
            height: 16,
            layout: packed_pixel_layout(VideoFormat::Bgra).unwrap(),
        };

        draw_packed_text_centered(&mut surface, 8.0, 8.0, "0.9", 0xFFAA_BBCC);

        assert!(pixels.chunks_exact(4).any(|px| px != [0, 0, 0, 0]));
    }

    #[test]
    fn packed_circle_draws_pixels_on_bgra_buffer() {
        let mut pixels = vec![0_u8; 16 * 16 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 16 * 4,
            width: 16,
            height: 16,
            layout: packed_pixel_layout(VideoFormat::Bgra).unwrap(),
        };

        draw_packed_circle(&mut surface, 8.0, 8.0, 3.0, 0xFF11_2233);

        assert_ne!(pixel_at(&pixels, 16 * 4, 8, 8), [0, 0, 0, 0]);
    }

    #[test]
    fn packed_line_draws_pixels_on_bgra_buffer() {
        let mut pixels = vec![0_u8; 32 * 16 * 4];
        let mut surface = PackedSurface {
            data: &mut pixels,
            stride: 32 * 4,
            width: 32,
            height: 16,
            layout: packed_pixel_layout(VideoFormat::Bgra).unwrap(),
        };

        draw_packed_line(&mut surface, 2.0, 2.0, 20.0, 2.0, 0xFFAA_BBCC, 2.0);

        assert_ne!(pixel_at(&pixels, 32 * 4, 10, 2), [0, 0, 0, 0]);
    }
}
