// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Visual demo of `segbackgroundblur`: builds a synthetic frame (a circular
//! "object" of sharp red stripes over a white/black striped background) with a
//! matching segmentation mask, runs it through the bin, and saves before/after
//! PNGs. The output should show the object kept sharp and un-tinted while the
//! background stripes blur toward grey.
//!
//! Run: `cargo run --example segbackgroundblur_save -- [out_dir]`

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use gst_video::prelude::*;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

const W: u32 = 320;
const H: u32 = 240;
const STRIDE: usize = W as usize * 4;
const CX: i32 = 160;
const CY: i32 = 120;
const R: i32 = 70;

fn make_mask_buffer(width: u32, height: u32, values: Vec<u8>) -> gst::Buffer {
    let mut mask = gst::Buffer::from_mut_slice(values);
    gst_video::VideoMeta::add(
        mask.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Gray8,
        width,
        height,
    )
    .unwrap();
    mask
}

fn add_segmentation_mtd(
    relation: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    mask: gst::Buffer,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) {
    let mut region_ids = vec![0_u32, 1_u32];
    let mut seg_mtd =
        std::mem::MaybeUninit::<gst_analytics::ffi::GstAnalyticsSegmentationMtd>::uninit();
    let ok: bool = unsafe {
        from_glib(
            gst_analytics::ffi::gst_analytics_relation_meta_add_segmentation_mtd(
                relation.as_mut_ptr(),
                mask.into_glib_ptr(),
                gst_analytics::ffi::GST_SEGMENTATION_TYPE_SEMANTIC,
                region_ids.len(),
                region_ids.as_mut_ptr(),
                x,
                y,
                w,
                h,
                seg_mtd.as_mut_ptr(),
            ),
        )
    };
    assert!(ok, "failed to add segmentation metadata");
}

fn in_circle(x: i32, y: i32) -> bool {
    let dx = x - CX;
    let dy = y - CY;
    dx * dx + dy * dy <= R * R
}

/// Scene pixels (RGBA, alpha 255): sharp red stripes inside the object circle,
/// white/black stripes everywhere else.
fn scene() -> Vec<u8> {
    let mut data = vec![0u8; STRIDE * H as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let px = &mut data[y * STRIDE + x * 4..y * STRIDE + x * 4 + 4];
            let stripe = (x % 8) < 4;
            let rgba: [u8; 4] = if in_circle(x as i32, y as i32) {
                // Object: red / dark-red stripes.
                if stripe {
                    [230, 30, 30, 255]
                } else {
                    [90, 0, 0, 255]
                }
            } else {
                // Background: white / black stripes (high frequency → blurs).
                if stripe {
                    [245, 245, 245, 255]
                } else {
                    [10, 10, 10, 255]
                }
            };
            px.copy_from_slice(&rgba);
        }
    }
    data
}

fn make_frame() -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(scene());
    {
        let b = buffer.get_mut().unwrap();
        b.set_pts(gst::ClockTime::ZERO);
        b.set_duration(gst::ClockTime::from_mseconds(33));
        gst_video::VideoMeta::add(
            b,
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Rgba,
            W,
            H,
        )
        .unwrap();

        // Mask over the circle's bounding box: native 96×96, value 1 inside the
        // circle (mapped into the mask's own coordinate space).
        let n = 96usize;
        let mut mask = vec![0u8; n * n];
        for my in 0..n {
            for mx in 0..n {
                let fx = (CX - R) + (mx as i32 * (2 * R) / n as i32);
                let fy = (CY - R) + (my as i32 * (2 * R) / n as i32);
                if in_circle(fx, fy) {
                    mask[my * n + mx] = 1;
                }
            }
        }
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(b);
        add_segmentation_mtd(
            &mut relation,
            make_mask_buffer(n as u32, n as u32, mask),
            CX - R,
            CY - R,
            (2 * R) as u32,
            (2 * R) as u32,
        );
    }
    buffer
}

fn save_png(path: &Path, rgba_tight: &[u8]) {
    let file = File::create(path).unwrap();
    let mut enc = png::Encoder::new(BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .unwrap()
        .write_image_data(rgba_tight)
        .unwrap();
    println!("wrote {}", path.display());
}

/// Read an RGBA buffer into a tight (no stride padding) byte vector.
fn tight_rgba(buffer: &gst::BufferRef) -> Vec<u8> {
    let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, W, H)
        .build()
        .unwrap();
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).unwrap();
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).unwrap();
    let mut out = vec![0u8; STRIDE * H as usize];
    for y in 0..H as usize {
        out[y * STRIDE..y * STRIDE + STRIDE]
            .copy_from_slice(&data[y * stride..y * stride + STRIDE]);
    }
    out
}

fn main() {
    gst::init().unwrap();
    gstoverlays::plugin_register_static().unwrap();

    let out_dir = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let out_dir = Path::new(&out_dir);

    // Save the input scene for comparison.
    save_png(&out_dir.join("segbackgroundblur_input.png"), &scene());

    let pipeline = gst::Pipeline::new();
    let src = gst_app::AppSrc::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .width(W as i32)
                .height(H as i32)
                .framerate(gst::Fraction::new(30, 1))
                .build(),
        )
        .format(gst::Format::Time)
        .is_live(false)
        .build();
    let bin = gst::ElementFactory::make("segbackgroundblur")
        .property("selected-types", "")
        .property("feather", 4u32)
        .property("sigma", 8.0f64)
        .build()
        .unwrap();
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .build(),
        )
        .build();

    pipeline
        .add_many([src.upcast_ref::<gst::Element>(), &bin, sink.upcast_ref()])
        .unwrap();
    gst::Element::link_many([src.upcast_ref::<gst::Element>(), &bin, sink.upcast_ref()]).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    for i in 0..5u64 {
        let mut buf = make_frame();
        buf.get_mut()
            .unwrap()
            .set_pts(gst::ClockTime::from_mseconds(i * 33));
        src.push_buffer(buf).unwrap();
    }
    src.end_of_stream().unwrap();

    let mut last = None;
    while let Ok(sample) = sink.pull_sample() {
        last = Some(sample);
    }
    let _ = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );

    let sample = last.expect("no output frame");
    save_png(
        &out_dir.join("segbackgroundblur_output.png"),
        &tight_rgba(sample.buffer().unwrap()),
    );

    pipeline.set_state(gst::State::Null).unwrap();
    println!("done");
}
