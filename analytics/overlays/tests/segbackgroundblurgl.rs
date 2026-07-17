// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! GL end-to-end test for `segbackgroundblurgl`. Requires a working GL context,
//! so it is `#[ignore]`d by default (headless CI has none); run on a GL machine
//! with `cargo test -p gst-plugin-overlays --features gl -- --ignored`.

#![cfg(feature = "gl")]

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use std::sync::Once;

const W: u32 = 64;
const H: u32 = 64;
const STRIDE: usize = W as usize * 4;
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
                OX as i32,
                OY as i32,
                OW as u32,
                OH as u32,
                seg_mtd.as_mut_ptr(),
            ),
        )
    };
    assert!(ok, "failed to add segmentation metadata");
}

fn make_frame(pts: gst::ClockTime) -> gst::Buffer {
    let mut data = vec![0u8; STRIDE * H as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let px = &mut data[y * STRIDE + x * 4..y * STRIDE + x * 4 + 4];
            let in_obj = (OX..OX + OW).contains(&x) && (OY..OY + OH).contains(&y);
            if in_obj {
                px.copy_from_slice(&[255, 0, 0, 255]);
            } else if (x % 4) < 2 {
                px.copy_from_slice(&[255, 255, 255, 255]);
            } else {
                px.copy_from_slice(&[0, 0, 0, 255]);
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
        add_segmentation_mtd(&mut relation, make_mask_buffer(8, 8, vec![1u8; 64]));
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

/// Drive a GLMemory pipeline through `factory` (the GL bin, or the auto-bin which
/// must select the GL child on GLMemory input) and assert the effect.
fn run_gl_effect(factory: &str) {
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
    let upload = gst::ElementFactory::make("glupload").build().unwrap();
    let bin = gst::ElementFactory::make(factory).build().unwrap();
    let download = gst::ElementFactory::make("gldownload").build().unwrap();
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgba)
                .build(),
        )
        .build();

    pipeline
        .add_many([
            src.upcast_ref::<gst::Element>(),
            &upload,
            &bin,
            &download,
            sink.upcast_ref(),
        ])
        .unwrap();
    gst::Element::link_many([
        src.upcast_ref::<gst::Element>(),
        &upload,
        &bin,
        &download,
        sink.upcast_ref(),
    ])
    .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();
    for i in 0..5u64 {
        src.push_buffer(make_frame(gst::ClockTime::from_mseconds(i * 33)))
            .unwrap();
    }
    src.end_of_stream().unwrap();

    // Drain every frame (the first may precede the blur branch priming) and keep
    // the last, then wait for EOS so all GL work finishes before teardown —
    // otherwise the process can exit mid-upload and crash in the GL stack.
    let mut last = None;
    while let Ok(sample) = sink.pull_sample() {
        last = Some(sample);
    }
    let sample = last.expect("bin produced no output frame");
    let _ = pipeline.bus().unwrap().timed_pop_filtered(
        gst::ClockTime::from_seconds(5),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );

    let [r, g, b, _] = pixel(&sample, OX + OW / 2, OY + OH / 2);
    assert!(
        r > 150 && g < 100 && b < 100,
        "object centre should stay sharp red, got {r},{g},{b}"
    );

    let [r, g, b, _] = pixel(&sample, 4, 32);
    assert!(
        (30..=225).contains(&r) && (30..=225).contains(&g) && (30..=225).contains(&b),
        "background stripes should be blurred toward grey, got {r},{g},{b}"
    );

    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
#[ignore = "requires a GL context"]
fn gl_background_is_blurred_and_object_stays_sharp() {
    init();
    run_gl_effect("segbackgroundblurgl");
}

#[test]
#[ignore = "requires a GL context"]
fn gl_autobin_selects_gl_child_and_blurs() {
    init();
    // GLMemory input: the auto-bin must select the GL child.
    run_gl_effect("segbackgroundblurbin");
}
