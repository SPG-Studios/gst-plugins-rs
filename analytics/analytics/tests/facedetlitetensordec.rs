// Copyright (C) 2026 Collabora Ltd
//  @author:  Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use byte_slice_cast::*;
use gst_analytics::AnalyticsMetaRefExt;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsanalytics::plugin_register_static().unwrap();
    });
}

/// Build a GstBuffer carrying a TensorMeta with three tensors matching the
/// FaceDetLite output format for a `fm_h × fm_w` feature map.
///
/// The heatmap has a single peak at (peak_cy, peak_cx) with value `peak_logit`
/// (pre-sigmoid). All other cells are -10.0 (sigmoid ≈ 0.0). The box offsets
/// at the peak cell give a box of `box_size × box_size` centred on the cell.
/// Landmark offsets are set so all five landmarks land on the cell centre.
fn make_face_det_buffer(
    fm_h: usize,
    fm_w: usize,
    peak_cy: usize,
    peak_cx: usize,
    peak_logit: f32,
    box_size: f32,
) -> gst::Buffer {
    let n_cells = fm_h * fm_w;
    let stride = 8.0f32;

    // Heatmap: [1, fm_h, fm_w, 1]
    let mut hm = vec![-10.0f32; n_cells];
    hm[peak_cy * fm_w + peak_cx] = peak_logit;

    // Boxes: [1, fm_h, fm_w, 4] — offsets in feature-map units
    // half the box size (in pixel space) / stride = half_size_fm
    let half_size_fm = (box_size / 2.0) / stride;
    let mut boxes = vec![0.0f32; n_cells * 4];
    let cell = peak_cy * fm_w + peak_cx;
    boxes[cell * 4] = half_size_fm; // left offset
    boxes[cell * 4 + 1] = half_size_fm; // top offset
    boxes[cell * 4 + 2] = half_size_fm; // right offset
    boxes[cell * 4 + 3] = half_size_fm; // bottom offset

    // Landmarks: [1, fm_h, fm_w, 10] — zero offsets → land on cell centre
    let landmarks = vec![0.0f32; n_cells * 10];

    let make_tensor = |id: &str, data: &[f32], dims: &[usize]| {
        let bytes = data.as_byte_slice();
        let buf = {
            let mut b = gst::Buffer::with_size(bytes.len()).unwrap();
            {
                let bref = b.get_mut().unwrap();
                let mut map = bref.map_writable().unwrap();
                map.copy_from_slice(bytes);
            }
            b
        };
        gst_analytics::Tensor::new_simple(
            glib::Quark::from_str(id),
            gst_analytics::TensorDataType::Float32,
            buf,
            gst_analytics::TensorDimOrder::RowMajor,
            dims,
        )
    };

    let hm_tensor = make_tensor(
        "face-det-lite-out-heatmap",
        &hm,
        &[1, fm_h, fm_w, 1],
    );
    let bx_tensor = make_tensor(
        "face-det-lite-out-boxes",
        &boxes,
        &[1, fm_h, fm_w, 4],
    );
    let lm_tensor = make_tensor(
        "face-det-lite-out-landmarks",
        &landmarks,
        &[1, fm_h, fm_w, 10],
    );

    let mut buffer = gst::Buffer::new();
    {
        let bref = buffer.get_mut().unwrap();
        let mut tmeta = gst_analytics::TensorMeta::add(bref);
        tmeta.set([hm_tensor, bx_tensor, lm_tensor].into());
    }
    buffer
}

#[test]
fn detects_single_face() {
    init();

    let dec = gst::ElementFactory::make("facedetlitetensordec")
        .property("confidence-threshold", 0.3f32)
        .property("nms-iou-threshold", 0.5f32)
        .build()
        .unwrap();

    let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, 128, 128)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .unwrap()
        .to_caps()
        .unwrap();

    let mut h = gst_check::Harness::with_element(&dec, Some("sink"), Some("src"));
    h.set_src_caps(caps);
    h.play();

    // Place a single strong peak in the centre of the 16×16 feature map
    let fm_h = 16usize;
    let fm_w = 16usize;
    let peak_cy = 8usize;
    let peak_cx = 8usize;
    let peak_logit = 5.0f32; // sigmoid(5) ≈ 0.993 >> 0.3 threshold

    let buffer = make_face_det_buffer(fm_h, fm_w, peak_cy, peak_cx, peak_logit, 40.0);
    assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

    let out = h.pull().unwrap();

    // Verify we got exactly one OD metadata entry
    let rmeta = out
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("AnalyticsRelationMeta must be present");

    let od_ids: Vec<u32> = rmeta
        .iter::<gst_analytics::AnalyticsODMtd>()
        .map(|m: gst_analytics::AnalyticsMtdRef<'_, gst_analytics::AnalyticsODMtd>| m.id())
        .collect();

    assert_eq!(od_ids.len(), 1, "Expected exactly one face OD metadata");

    let od = rmeta.mtd::<gst_analytics::AnalyticsODMtd>(od_ids[0]).unwrap();
    let location = od.location().unwrap();
    // Cell (8,8) → pixel centre at (64, 64). Box half-size = 20px.
    // expected: x=44, y=44, w=40, h=40 (approximately)
    assert!(
        location.x >= 40 && location.x <= 50,
        "Unexpected x={}", location.x
    );
    assert!(
        location.y >= 40 && location.y <= 50,
        "Unexpected y={}", location.y
    );
    assert!(
        location.w >= 35 && location.w <= 45,
        "Unexpected w={}", location.w
    );
    assert!(
        location.h >= 35 && location.h <= 45,
        "Unexpected h={}", location.h
    );

    // Verify the OD metadata has a CONTAIN relation to a keypoint group
    let related: Vec<_> = rmeta
        .iter_direct_related::<gst_analytics::AnalyticsGroupMtd>(
            od_ids[0],
            gst_analytics::RelTypes::CONTAIN,
        )
        .collect();
    assert_eq!(related.len(), 1, "Expected exactly one keypoint group");

    let group = &related[0];
    assert_eq!(group.member_count(), 5, "Expected 5 facial landmark keypoints");
}

#[test]
fn no_detection_below_threshold() {
    init();

    let dec = gst::ElementFactory::make("facedetlitetensordec")
        .property("confidence-threshold", 0.9f32)
        .build()
        .unwrap();

    let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, 128, 128)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .unwrap()
        .to_caps()
        .unwrap();

    let mut h = gst_check::Harness::with_element(&dec, Some("sink"), Some("src"));
    h.set_src_caps(caps);
    h.play();

    // sigmoid(1.0) ≈ 0.73, below 0.9 threshold
    let buffer = make_face_det_buffer(16, 16, 4, 4, 1.0f32, 40.0);
    assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

    let out = h.pull().unwrap();
    assert!(
        out.meta::<gst_analytics::AnalyticsRelationMeta>().is_none(),
        "No analytics metadata expected when score is below threshold"
    );
}

#[test]
fn nms_suppresses_duplicate_faces() {
    init();

    let dec = gst::ElementFactory::make("facedetlitetensordec")
        .property("confidence-threshold", 0.3f32)
        .property("nms-iou-threshold", 0.5f32)
        .build()
        .unwrap();

    let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, 128, 128)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .unwrap()
        .to_caps()
        .unwrap();

    let mut h = gst_check::Harness::with_element(&dec, Some("sink"), Some("src"));
    h.set_src_caps(caps);
    h.play();

    let fm_h = 16usize;
    let fm_w = 16usize;
    let n_cells = fm_h * fm_w;

    // Two adjacent peaks at (8,8) and (8,9) — they will produce heavily overlapping boxes
    let peak_logit = 5.0f32;
    let mut hm = vec![-10.0f32; n_cells];
    hm[8 * fm_w + 8] = peak_logit;
    hm[8 * fm_w + 9] = peak_logit - 0.1; // slightly lower so first one wins

    let half_size_fm = (64.0f32 / 2.0) / 8.0; // big boxes → overlap
    let mut boxes = vec![0.0f32; n_cells * 4];
    for &cell in &[8 * fm_w + 8, 8 * fm_w + 9] {
        boxes[cell * 4] = half_size_fm;
        boxes[cell * 4 + 1] = half_size_fm;
        boxes[cell * 4 + 2] = half_size_fm;
        boxes[cell * 4 + 3] = half_size_fm;
    }
    let landmarks = vec![0.0f32; n_cells * 10];

    let make_tensor = |id: &str, data: &[f32], dims: &[usize]| {
        let bytes = data.as_byte_slice();
        let buf = {
            let mut b = gst::Buffer::with_size(bytes.len()).unwrap();
            {
                let bref = b.get_mut().unwrap();
                let mut map = bref.map_writable().unwrap();
                map.copy_from_slice(bytes);
            }
            b
        };
        gst_analytics::Tensor::new_simple(
            glib::Quark::from_str(id),
            gst_analytics::TensorDataType::Float32,
            buf,
            gst_analytics::TensorDimOrder::RowMajor,
            dims,
        )
    };

    let mut buffer = gst::Buffer::new();
    {
        let bref = buffer.get_mut().unwrap();
        let mut tmeta = gst_analytics::TensorMeta::add(bref);
        tmeta.set(
            [
                make_tensor("face-det-lite-out-heatmap", &hm, &[1, fm_h, fm_w, 1]),
                make_tensor("face-det-lite-out-boxes", &boxes, &[1, fm_h, fm_w, 4]),
                make_tensor("face-det-lite-out-landmarks", &landmarks, &[1, fm_h, fm_w, 10]),
            ]
            .into(),
        );
    }

    assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));
    let out = h.pull().unwrap();

    let rmeta = out
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("AnalyticsRelationMeta must be present");

    let count = rmeta.iter::<gst_analytics::AnalyticsODMtd>().count();
    assert_eq!(count, 1, "NMS should suppress the duplicate detection, got {count}");
}

#[test]
fn v2_variant_detection() {
    init();

    let dec = gst::ElementFactory::make("facedetlitetensordec")
        .property("confidence-threshold", 0.3f32)
        .property("nms-iou-threshold", 0.5f32)
        .build()
        .unwrap();

    let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, 128, 128)
        .fps(gst::Fraction::new(30, 1))
        .build()
        .unwrap()
        .to_caps()
        .unwrap();

    let mut h = gst_check::Harness::with_element(&dec, Some("sink"), Some("src"));
    h.set_src_caps(caps);
    h.play();

    let fm_h = 16usize;
    let fm_w = 16usize;
    let peak_cy = 8usize;
    let peak_cx = 8usize;
    let peak_logit = 5.0f32;
    let box_size = 40.0f32;
    let n_cells = fm_h * fm_w;
    let stride = 8.0f32;

    // v2 tensors are in NCHW (channel-first) layout, matching the ONNX export.
    // Heatmap: [1, 1, fm_h, fm_w]
    let mut hm = vec![-10.0f32; n_cells];
    hm[peak_cy * fm_w + peak_cx] = peak_logit;

    let half_size_fm = (box_size / 2.0) / stride;

    // Boxes (NCHW): [1, 4, fm_h, fm_w]. Each channel is a full feature map.
    let mut boxes = vec![0.0f32; 4 * n_cells];
    let plane = n_cells;
    let cell = peak_cy * fm_w + peak_cx;
    boxes[0 * plane + cell] = half_size_fm; // left
    boxes[1 * plane + cell] = half_size_fm; // top
    boxes[2 * plane + cell] = half_size_fm; // right
    boxes[3 * plane + cell] = half_size_fm; // bottom

    // Landmarks (NCHW): [1, 10, fm_h, fm_w]. Zero offsets → landmarks at cell centre.
    let landmarks = vec![0.0f32; 10 * n_cells];

    let make_tensor = |id: &str, data: &[f32], dims: &[usize]| {
        let bytes = data.as_byte_slice();
        let buf = {
            let mut b = gst::Buffer::with_size(bytes.len()).unwrap();
            {
                let bref = b.get_mut().unwrap();
                let mut map = bref.map_writable().unwrap();
                map.copy_from_slice(bytes);
            }
            b
        };
        gst_analytics::Tensor::new_simple(
            glib::Quark::from_str(id),
            gst_analytics::TensorDataType::Float32,
            buf,
            gst_analytics::TensorDimOrder::RowMajor,
            dims,
        )
    };

    // Use v2 tensor IDs (NCHW layout)
    let hm_tensor = make_tensor(
        "face-det-lite-out-v2-heatmap",
        &hm,
        &[1, 1, fm_h, fm_w],
    );
    let bx_tensor = make_tensor(
        "face-det-lite-out-v2-boxes",
        &boxes,
        &[1, 4, fm_h, fm_w],
    );
    let lm_tensor = make_tensor(
        "face-det-lite-out-v2-landmarks",
        &landmarks,
        &[1, 10, fm_h, fm_w],
    );

    let mut buffer = gst::Buffer::new();
    {
        let bref = buffer.get_mut().unwrap();
        let mut tmeta = gst_analytics::TensorMeta::add(bref);
        tmeta.set([hm_tensor, bx_tensor, lm_tensor].into());
    }

    assert_eq!(h.push(buffer), Ok(gst::FlowSuccess::Ok));

    let out = h.pull().unwrap();

    // Verify we got exactly one OD metadata entry (element correctly detected v2 variant)
    let rmeta = out
        .meta::<gst_analytics::AnalyticsRelationMeta>()
        .expect("AnalyticsRelationMeta must be present for v2 variant");

    let od_ids: Vec<u32> = rmeta
        .iter::<gst_analytics::AnalyticsODMtd>()
        .map(|m: gst_analytics::AnalyticsMtdRef<'_, gst_analytics::AnalyticsODMtd>| m.id())
        .collect();

    assert_eq!(od_ids.len(), 1, "Expected exactly one face OD metadata from v2 variant");

    let od = rmeta.mtd::<gst_analytics::AnalyticsODMtd>(od_ids[0]).unwrap();
    let location = od.location().unwrap();
    assert!(
        location.x >= 40 && location.x <= 50,
        "Unexpected x={}", location.x
    );
    assert!(
        location.y >= 40 && location.y <= 50,
        "Unexpected y={}", location.y
    );

    // Verify the OD metadata has a CONTAIN relation to a keypoint group
    let related: Vec<_> = rmeta
        .iter_direct_related::<gst_analytics::AnalyticsGroupMtd>(
            od_ids[0],
            gst_analytics::RelTypes::CONTAIN,
        )
        .collect();
    assert_eq!(
        related.len(),
        1,
        "Expected exactly one keypoint group from v2 variant"
    );
}
