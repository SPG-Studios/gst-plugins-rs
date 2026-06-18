// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Performance sanity benchmark driver for the overlay elements.
//
// Times steady-state per-frame processing for one overlay element through
// `gst_check::Harness`, against whichever plugin provides the factory on
// `GST_PLUGIN_PATH`. The same binary therefore measures both the C
// (`analyticsoverlay`) and the Rust (`gstoverlays`) implementations — select
// one by pointing `GST_PLUGIN_PATH` at its `.so` and *not* the other's.
//
// It reports per-frame timing with rendering on and off; the difference
// isolates the actual draw work from buffer/harness overhead (which is
// identical for both implementations). The same driver is used under
// `valgrind --tool=callgrind` for hotspot attribution.
//
// Usage:
//   overlay_bench <odoverlay|segoverlay|keypointsoverlay> [width] [height] [iters]

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;

use std::time::{Duration, Instant};

fn warmup() -> usize {
    std::env::var("BENCH_WARMUP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200)
}

fn caps(width: i32, height: i32) -> String {
    format!("video/x-raw,format=BGRA,width={width},height={height},framerate=30/1")
}

fn blank_buffer(width: i32, height: i32) -> gst::Buffer {
    let size = (width * height * 4) as usize;
    let mut buffer = gst::Buffer::from_mut_slice(vec![0u8; size]);
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    buffer
}

/// 8 detection boxes (one overlapping pair) with class labels — the richest
/// renderer: rectangles, label text, leader lines, placement avoidance.
fn attach_od(r: &mut gst::BufferRef, _width: i32, _height: i32) {
    use gst_analytics::AnalyticsRelationMetaODExt;
    let mut meta = gst_analytics::AnalyticsRelationMeta::add(r);
    let boxes = [
        ("person", 80, 60, 180, 360, 0.94),
        ("car", 520, 200, 360, 220, 0.88),
        ("car", 700, 280, 360, 220, 0.81),
        ("dog", 200, 520, 220, 160, 0.76),
        ("bicycle", 1200, 120, 260, 300, 0.69),
        ("bus", 1400, 500, 420, 300, 0.91),
        ("person", 980, 640, 150, 360, 0.85),
        ("traffic light", 460, 40, 60, 140, 0.72),
    ];
    for (label, x, y, w, h, conf) in boxes {
        meta.add_od_mtd(glib::Quark::from_str(label), x, y, w, h, conf)
            .unwrap();
    }
}

/// A 21-point hand with the specialised skeleton renderer.
fn attach_kp(r: &mut gst::BufferRef, width: i32, height: i32) {
    use gst_analytics::{
        AnalyticsKeypointDimensions, AnalyticsKeypointPosition, AnalyticsRelationMetaGroupExt,
    };
    // A hand splayed across the centre of the frame, scaled to the resolution.
    let base = [
        (160, 215),
        (120, 200),
        (100, 180),
        (88, 160),
        (80, 140),
        (135, 150),
        (130, 120),
        (127, 100),
        (125, 82),
        (160, 145),
        (160, 112),
        (160, 90),
        (160, 70),
        (185, 150),
        (190, 120),
        (193, 100),
        (195, 82),
        (208, 160),
        (216, 135),
        (221, 118),
        (225, 102),
    ];
    let sx = width as f32 / 320.0;
    let sy = height as f32 / 240.0;
    let mut meta = gst_analytics::AnalyticsRelationMeta::add(r);
    let positions = base
        .iter()
        .map(|(x, y)| AnalyticsKeypointPosition {
            x: (*x as f32 * sx) as i32,
            y: (*y as f32 * sy) as i32,
            z: 0,
            dimension: AnalyticsKeypointDimensions::_2d,
        })
        .collect::<Vec<_>>();
    meta.add_keypoints_group_from_positions("hand-kp-21", &positions, None, None, &[])
        .unwrap();
}

fn add_seg_mtd(
    relation: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    mask: gst::Buffer,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) {
    let mut region_ids = vec![0u32, 1, 2, 3];
    let mut seg =
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
                seg.as_mut_ptr(),
            ),
        )
    };
    assert!(ok, "failed to add segmentation mtd");
}

fn mask_buffer(w: u32, h: u32, value: u8) -> gst::Buffer {
    let mut mask = gst::Buffer::from_mut_slice(vec![value; (w * h) as usize]);
    gst_video::VideoMeta::add(
        mask.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Gray8,
        w,
        h,
    )
    .unwrap();
    mask
}

/// One full-frame mask plus two smaller regions — mask scaling + compositing.
/// The full-frame mask's native size is `BENCH_SEG_MASK` (default 16, an extreme
/// upscale; set e.g. 256 for a model-typical mask).
fn attach_seg(r: &mut gst::BufferRef, width: i32, height: i32) {
    let full = std::env::var("BENCH_SEG_MASK")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(16);
    let region = (full / 2).max(8);
    let mut meta = gst_analytics::AnalyticsRelationMeta::add(r);
    add_seg_mtd(
        &mut meta,
        mask_buffer(full, full, 1),
        0,
        0,
        width as u32,
        height as u32,
    );
    add_seg_mtd(
        &mut meta,
        mask_buffer(region, region, 2),
        width / 8,
        height / 8,
        (width / 3) as u32,
        (height / 3) as u32,
    );
    add_seg_mtd(
        &mut meta,
        mask_buffer(region, region, 3),
        width / 2,
        height / 2,
        (width / 3) as u32,
        (height / 3) as u32,
    );
}

/// Map either implementation's factory name to a scene. Rust factories are
/// `odoverlay`/`segoverlay`/`keypointsoverlay`; the C ones are
/// `objectdetectionoverlay`/`segmentationoverlay`/`keypointoverlay`.
fn attach_scene(factory: &str, r: &mut gst::BufferRef, width: i32, height: i32) {
    if factory.contains("keypoint") {
        attach_kp(r, width, height)
    } else if factory.contains("seg") {
        attach_seg(r, width, height)
    } else {
        attach_od(r, width, height)
    }
}

fn build_scene(factory: &str, width: i32, height: i32) -> gst::Buffer {
    let mut buffer = blank_buffer(width, height);
    attach_scene(factory, buffer.get_mut().unwrap(), width, height);
    buffer
}

struct Stats {
    mean: Duration,
    median: Duration,
    p95: Duration,
    min: Duration,
}

fn summarize(mut d: Vec<Duration>) -> Stats {
    d.sort_unstable();
    let n = d.len();
    let sum: Duration = d.iter().sum();
    Stats {
        mean: sum / n as u32,
        median: d[n / 2],
        p95: d[(n * 95 / 100).min(n - 1)],
        min: d[0],
    }
}

fn bench(factory: &str, width: i32, height: i32, iters: usize, render: bool) -> Stats {
    let mut harness = gst_check::Harness::new(factory);
    harness.set_src_caps_str(&caps(width, height));
    harness.set_sink_caps_str(&caps(width, height));
    // `render-enabled` is a Rust-element property; the C elements always render.
    let element = harness.element().unwrap();
    if element.find_property("render-enabled").is_some() {
        element.set_property("render-enabled", render);
    }

    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(harness.push_event(gst::event::Segment::builder(&segment).build()));

    let warmup = warmup();
    let mut durations = Vec::with_capacity(iters);
    for i in 0..(warmup + iters) {
        let buf = build_scene(factory, width, height);
        let t = Instant::now();
        harness.push(buf).unwrap();
        let out = harness.pull().unwrap();
        let dt = t.elapsed();
        drop(out);
        if i >= warmup {
            durations.push(dt);
        }
    }
    summarize(durations)
}

/// Representative-pipeline throughput: videotestsrc -> overlay -> fakesink,
/// driven to EOS, with synthetic analytics meta attached to every frame by a
/// pad probe. videotestsrc (not appsrc) drives the flow to avoid the
/// push-backpressure/preroll deadlock the C elements hit. Returns frames per
/// second over the whole run (overlay dominates; ~100 warmup frames included).
fn run_pipeline(factory: &str, width: i32, height: i32, iters: usize) -> f64 {
    let total = (WARMUP_PIPELINE + iters) as i32;
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", total)
        .property("is-live", false)
        .build()
        .unwrap();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps(width, height).parse::<gst::Caps>().unwrap())
        .build()
        .unwrap();
    let overlay = gst::ElementFactory::make(factory).build().unwrap();
    if overlay.find_property("render-enabled").is_some() {
        overlay.set_property("render-enabled", true);
    }
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .unwrap();
    pipeline
        .add_many([&src, &capsfilter, &overlay, &sink])
        .unwrap();
    gst::Element::link_many([&src, &capsfilter, &overlay, &sink]).unwrap();

    // Attach synthetic analytics meta to every frame on the overlay sink pad.
    let f = factory.to_string();
    let pad = overlay.static_pad("sink").unwrap();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data {
            attach_scene(&f, buffer.make_mut(), width, height);
        }
        gst::PadProbeReturn::Ok
    });

    let start = Instant::now();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    match bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(120),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(msg) => {
            if let gst::MessageView::Error(e) = msg.view() {
                panic!("pipeline error: {} ({:?})", e.error(), e.debug());
            }
        }
        None => panic!("pipeline timed out before EOS"),
    }
    let elapsed = start.elapsed();
    pipeline.set_state(gst::State::Null).unwrap();

    total as f64 / elapsed.as_secs_f64()
}

const WARMUP_PIPELINE: usize = 100;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let factory = args.get(1).map(|s| s.as_str()).unwrap_or("odoverlay");
    let width: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1920);
    let height: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1080);
    let iters: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let pipeline_mode = args.iter().any(|a| a == "--pipeline");

    gst::init().unwrap();

    if pipeline_mode {
        let element = gst::ElementFactory::make(factory)
            .build()
            .unwrap_or_else(|_| panic!("factory {factory} not found on GST_PLUGIN_PATH"));
        let type_name = element.type_().name().to_string();
        drop(element);
        let fps = run_pipeline(factory, width, height, iters);
        println!("element     : {factory} ({type_name})");
        println!(
            "pipeline    : videotestsrc -> {factory} -> fakesink, {width}x{height}, {iters} frames"
        );
        println!(
            "throughput  : {fps:8.1} fps  ({:.3} ms/frame)",
            1000.0 / fps
        );
        return;
    }

    // Report which implementation is actually being measured.
    let probe = gst::ElementFactory::make(factory)
        .build()
        .unwrap_or_else(|_| panic!("factory {factory} not found on GST_PLUGIN_PATH"));
    let type_name = probe.type_().name().to_string();
    let has_render_toggle = probe.find_property("render-enabled").is_some();
    drop(probe);

    let us = |d: Duration| d.as_secs_f64() * 1e6;

    println!("element     : {factory} ({type_name})");
    println!(
        "frame       : {width}x{height} BGRA, {iters} iters (warmup {})",
        warmup()
    );

    let on = bench(factory, width, height, iters, true);
    println!(
        "render=on   : median {:8.2} us  mean {:8.2} us  p95 {:8.2} us  min {:8.2} us",
        us(on.median),
        us(on.mean),
        us(on.p95),
        us(on.min)
    );

    let render_only = std::env::var_os("BENCH_RENDER_ONLY").is_some();
    if has_render_toggle && !render_only {
        // Rust elements: subtract the no-draw path to isolate the draw work from
        // buffer/harness overhead.
        let off = bench(factory, width, height, iters, false);
        let render_cost = on.median.saturating_sub(off.median);
        println!(
            "render=off  : median {:8.2} us  mean {:8.2} us  p95 {:8.2} us  min {:8.2} us",
            us(off.median),
            us(off.mean),
            us(off.p95),
            us(off.min)
        );
        println!(
            "draw cost   : median {:8.2} us  (render=on minus render=off)",
            us(render_cost)
        );
    } else {
        // C elements render unconditionally; harness/buffer overhead is ~1 us
        // (see the Rust render=off figure), so render=on is effectively the draw cost.
        println!("render=off  : n/a (element always renders; overhead ~1 us)");
    }
}
