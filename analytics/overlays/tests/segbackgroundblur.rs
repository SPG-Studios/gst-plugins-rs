// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! End-to-end test for the `segbackgroundblur` bin: a synthetic frame (flat-red
//! object rect over high-frequency stripes) carrying a segmentation mask is run
//! through the bin, and the output is checked to show the object kept sharp while
//! the background is blurred. No ONNX model needed.

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use std::sync::Once;

const W: u32 = 64;
const H: u32 = 64;
const STRIDE: usize = W as usize * 4;
// Object rect (frame coords).
const OX: usize = 24;
const OY: usize = 24;
const OW: usize = 16;
const OH: usize = 16;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlays::plugin_register_static().expect("overlays test");
    });
}

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
    loc_x: i32,
    loc_y: i32,
    loc_w: u32,
    loc_h: u32,
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
                loc_x,
                loc_y,
                loc_w,
                loc_h,
                seg_mtd.as_mut_ptr(),
            ),
        )
    };
    assert!(ok, "failed to add segmentation metadata");
}

/// A frame: 2px vertical white/black stripes (high frequency → visibly blurs)
/// with a flat-red object rect, plus a segmentation mask over that rect.
fn make_frame(pts: gst::ClockTime) -> gst::Buffer {
    let mut data = vec![0u8; STRIDE * H as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let px = &mut data[y * STRIDE + x * 4..y * STRIDE + x * 4 + 4];
            let in_obj = (OX..OX + OW).contains(&x) && (OY..OY + OH).contains(&y);
            if in_obj {
                px.copy_from_slice(&[255, 0, 0, 255]); // red object
            } else if (x % 4) < 2 {
                px.copy_from_slice(&[255, 255, 255, 255]); // white stripe
            } else {
                px.copy_from_slice(&[0, 0, 0, 255]); // black stripe
            }
        }
    }
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let b = buffer.get_mut().unwrap();
        b.set_pts(pts);
        b.set_duration(gst::ClockTime::from_mseconds(33));
        gst_video::VideoMeta::add(
            b,
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Rgba,
            W,
            H,
        )
        .unwrap();
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(b);
        let mask = make_mask_buffer(8, 8, vec![1u8; 64]);
        add_segmentation_mtd(
            &mut relation,
            mask,
            OX as i32,
            OY as i32,
            OW as u32,
            OH as u32,
        );
    }
    buffer
}

fn pixel(sample: &gst::Sample, x: usize, y: usize) -> [u8; 4] {
    let buffer = sample.buffer().unwrap();
    let map = buffer.map_readable().unwrap();
    let off = y * STRIDE + x * 4;
    let s = &map.as_slice()[off..off + 4];
    [s[0], s[1], s[2], s[3]]
}

/// Run the effect through `factory` and assert the object stays sharp while the
/// background blurs. `factory` is either the CPU bin or the auto-selecting bin
/// (which, on system-memory input, must pick the CPU child).
fn run_effect(factory: &str) {
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
    let bin = gst::ElementFactory::make(factory).build().unwrap();
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

    // A few frames so the compositor has data to aggregate, then EOS.
    for i in 0..5u64 {
        src.push_buffer(make_frame(gst::ClockTime::from_mseconds(i * 33)))
            .unwrap();
    }
    src.end_of_stream().unwrap();

    let sample = sink
        .try_pull_sample(gst::ClockTime::from_seconds(10))
        .expect("bin produced no output frame");

    // Object centre: the sharp red foreground shows through the mask alpha.
    let [r, g, b, _] = pixel(&sample, OX + OW / 2, OY + OH / 2);
    assert!(
        r > 180 && g < 80 && b < 80,
        "object centre should stay sharp red, got {r},{g},{b}"
    );

    // Background (stripe region, well away from the object): the 2px white/black
    // stripes must have blurred toward mid-grey — i.e. no longer pure white/black.
    let [r, g, b, _] = pixel(&sample, 4, 32);
    assert!(
        (40..=210).contains(&r) && (40..=210).contains(&g) && (40..=210).contains(&b),
        "background stripes should be blurred toward grey, got {r},{g},{b}"
    );

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn background_is_blurred_and_object_stays_sharp() {
    init();
    run_effect("segbackgroundblur");
}

#[test]
fn autobin_selects_cpu_child_and_blurs_on_system_memory() {
    init();
    // On system-memory input the auto-bin must pick the CPU child and produce
    // the same effect.
    run_effect("segbackgroundblurbin");
}

#[test]
fn autobin_forwards_properties_before_a_child_exists() {
    init();
    let bin = gst::ElementFactory::make("segbackgroundblurbin")
        .build()
        .unwrap();

    // Forwarded from the CPU child; readable/writable before any child is built.
    bin.set_property("selected-types", "person");
    assert_eq!(
        bin.property::<Option<String>>("selected-types"),
        Some("person".to_string())
    );
    // sigma exists (CPU child has it); cached until a child is selected.
    bin.set_property("sigma", 9.0f64);
    assert_eq!(bin.property::<f64>("sigma"), 9.0);
}
