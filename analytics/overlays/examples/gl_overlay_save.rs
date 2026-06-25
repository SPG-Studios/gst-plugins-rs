// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Verification helper for the GL overlay elements: build a GL pipeline, attach
// real analytics metadata via a pad probe, and save one rendered frame.
//
//   gl_overlay_save <factory> [out.png]
//
// e.g. gl_overlay_save odoverlaygl /tmp/gl_real.png

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use gst_analytics::{
    AnalyticsKeypointDimensions, AnalyticsKeypointPosition, AnalyticsRelationMetaGroupExt,
    AnalyticsRelationMetaODExt,
};

/// Tag the keypoints element's 21-point hand renderer routes on.
const HAND_KP_21_TAG: &str = "hand-kp-21";

fn mask_buffer(w: u32, h: u32, values: Vec<u8>) -> gst::Buffer {
    let mut mask = gst::Buffer::from_mut_slice(values);
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

fn attach_segmentation(buffer: &mut gst::BufferRef) {
    // An 8x8 mask with a few distinct region values, placed across the frame.
    let mut values = vec![0u8; 64];
    for (i, v) in values.iter_mut().enumerate() {
        *v = ((i / 8) % 3 + 1) as u8; // bands of region ids 1,2,3
    }
    let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer);
    let mut region_ids = vec![0u32, 1, 2, 3];
    let mut seg =
        std::mem::MaybeUninit::<gst_analytics::ffi::GstAnalyticsSegmentationMtd>::uninit();
    let ok: bool = unsafe {
        from_glib(
            gst_analytics::ffi::gst_analytics_relation_meta_add_segmentation_mtd(
                relation.as_mut_ptr(),
                mask_buffer(8, 8, values).into_glib_ptr(),
                gst_analytics::ffi::GST_SEGMENTATION_TYPE_SEMANTIC,
                region_ids.len(),
                region_ids.as_mut_ptr(),
                100,
                80,
                1080,
                560,
                seg.as_mut_ptr(),
            ),
        )
    };
    assert!(ok, "failed to add segmentation mtd");
}

fn attach_od(buffer: &mut gst::BufferRef) {
    let mut meta = gst_analytics::AnalyticsRelationMeta::add(buffer);
    let boxes = [
        ("person", 60, 40, 120, 240, 0.94),
        ("car", 350, 130, 240, 150, 0.88),
        ("car", 470, 190, 240, 150, 0.81),
        ("dog", 130, 350, 150, 110, 0.76),
        ("bus", 900, 330, 280, 200, 0.91),
    ];
    for (label, x, y, w, h, conf) in boxes {
        meta.add_od_mtd(glib::Quark::from_str(label), x, y, w, h, conf)
            .unwrap();
    }
}

fn attach_keypoints(buffer: &mut gst::BufferRef) {
    // A 21-point hand laid out as a wrist + five fingers fanning upward, tagged
    // `hand-kp-21` so the specialized renderer draws the skeleton from its fixed
    // bone topology (with draw-skeleton=true, set on the element below).
    let wrist = (640, 600);
    let mut positions = vec![AnalyticsKeypointPosition {
        x: wrist.0,
        y: wrist.1,
        z: 0,
        dimension: AnalyticsKeypointDimensions::_2d,
    }];
    // Five fingers, four joints each, fanning out from the wrist.
    for finger in 0..5 {
        let dx = (finger - 2) * 110;
        for joint in 0..4 {
            let step = joint + 1;
            positions.push(AnalyticsKeypointPosition {
                x: wrist.0 + dx * step / 4,
                y: wrist.1 - 110 * step,
                z: 0,
                dimension: AnalyticsKeypointDimensions::_2d,
            });
        }
    }
    let mut meta = gst_analytics::AnalyticsRelationMeta::add(buffer);
    meta.add_keypoints_group_from_positions(HAND_KP_21_TAG, &positions, None, None, &[])
        .unwrap();
}

fn attach_meta(factory: &str, buffer: &mut gst::BufferRef) {
    if factory.contains("keypoint") {
        attach_keypoints(buffer);
    } else if factory.contains("seg") {
        attach_segmentation(buffer);
    } else {
        attach_od(buffer);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let factory = args.get(1).map(|s| s.as_str()).unwrap_or("odoverlaygl");
    let out = args
        .get(2)
        .map(|s| s.as_str())
        .unwrap_or("/tmp/gl_real.png");

    gst::init().unwrap();

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 1i32)
        .build()
        .unwrap();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            "video/x-raw,width=1280,height=720"
                .parse::<gst::Caps>()
                .unwrap(),
        )
        .build()
        .unwrap();
    let glupload = gst::ElementFactory::make("glupload").build().unwrap();
    let overlay = gst::ElementFactory::make(factory).build().unwrap();
    if factory.contains("keypoint") {
        // Exercise the skeleton path: draw the bones and select the hand group.
        overlay.set_property("draw-skeleton", true);
        overlay.set_property("semantic-tag", HAND_KP_21_TAG);
    }
    let gldownload = gst::ElementFactory::make("gldownload").build().unwrap();
    let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let pngenc = gst::ElementFactory::make("pngenc").build().unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", out)
        .build()
        .unwrap();

    pipeline
        .add_many([
            &src,
            &capsfilter,
            &glupload,
            &overlay,
            &gldownload,
            &convert,
            &pngenc,
            &sink,
        ])
        .unwrap();
    gst::Element::link_many([
        &src,
        &capsfilter,
        &glupload,
        &overlay,
        &gldownload,
        &convert,
        &pngenc,
        &sink,
    ])
    .unwrap();

    // Attach detection metadata to the buffer entering the overlay.
    let pad = overlay.static_pad("sink").unwrap();
    let f = factory.to_string();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
        if let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data {
            attach_meta(&f, buffer.make_mut());
        }
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    match bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(msg) => {
            if let gst::MessageView::Error(e) = msg.view() {
                panic!("pipeline error: {} ({:?})", e.error(), e.debug());
            }
        }
        None => panic!("timed out before EOS"),
    }
    pipeline.set_state(gst::State::Null).unwrap();
    println!("saved {out}");
}
