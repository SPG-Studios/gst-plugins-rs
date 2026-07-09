// Copyright (C) 2026 Collabora Ltd
//  @author Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-facedetlitetensordec
 * @see_also: objectdetectionoverlay, tfliteinference
 *
 * Tensor decoder for the [Qualcomm Lightweight Face Detector (FaceDetLite)](https://github.com/quic/ai-hub-models/tree/main/src/qai_hub_models/models/face_det_lite).
 *
 * This element consumes the three output tensors produced by the FaceDetLite model
 * (heatmap, boxes, landmarks) and attaches `GstAnalyticsRelationMeta` to each buffer,
 * containing:
 *
 * - An `AnalyticsODMtd` bounding box for every detected face
 * - An `AnalyticsGroupMtd` containing five `AnalyticsKeypointMtd` entries for the
 *   facial landmarks (left eye, right eye, nose, left mouth corner, right mouth corner),
 *   linked to the face bounding box via a `CONTAIN` relation
 *
 * The model is an anchor-free CenterNet-style detector accepting a single-channel
 * (grayscale) input.  All three output tensors share the spatial resolution
 * `FM_H × FM_W` where `FM_H = input_height / 8` and `FM_W = input_width / 8`.
 *
 * ## Tensor groups
 *
 * The element supports two variants of the FaceDetLite output tensors:
 *
 * ### v1 (TFLite variant)
 *
 * Tensor group named `face-det-lite-out`, containing:
 *
 * | Tensor ID | Shape | Description |
 * |---|---|---|
 * | `face-det-lite-out-heatmap` | 1 × FM_H × FM_W × 1 | Face-centre confidence logits |
 * | `face-det-lite-out-boxes` | 1 × FM_H × FM_W × 4 | Per-cell edge offsets (left, top, right, bottom) |
 * | `face-det-lite-out-landmarks` | 1 × FM_H × FM_W × 10 | Per-cell landmark offsets (x0…x4, y0…y4) |
 *
 * ### v2 (ONNX variant)
 *
 * Tensor group named `face-det-lite-out-v2`, containing:
 *
 * | Tensor ID | Shape | Description |
 * |---|---|---|
 * | `face-det-lite-out-v2-heatmap` | 1 × 1 × FM_H × FM_W | Face-centre confidence logits |
 * | `face-det-lite-out-v2-boxes` | 1 × 4 × FM_H × FM_W | Per-cell edge offsets (left, top, right, bottom) |
 * | `face-det-lite-out-v2-landmarks` | 1 × 10 × FM_H × FM_W | Per-cell landmark offsets (x0…x4, y0…y4) |
 *
 * The v2 variant uses NCHW (channel-first) layout matching the native ONNX
 * export of FaceDetLite.
 *
 * ## Example pipeline
 *
 * |[
 * gst-launch-1.0 filesrc location=photo.jpg ! jpegdec ! videoconvertscale \
 *   ! tfliteinference model-file=Lightweight-Face-Detection.tflite \
 *   ! facedetlitetensordec ! objectdetectionoverlay ! keypointoverlay \
 *   ! videoconvertscale ! autovideosink
 * ]|
 *
 * Since: plugins-rs-0.15.0
 */
use gst::{glib, subclass::prelude::*};
use gst_analytics::prelude::*;
use gst_video::{prelude::*, subclass::prelude::*};

use byte_slice_cast::*;

use std::sync::{LazyLock, Mutex};

const FACE_DET_LITE_OUT: &str = "face-det-lite-out";
const FACE_DET_LITE_OUT_V2: &str = "face-det-lite-out-v2";

// v1 (TFLite variant)
const FACE_DET_LITE_HEATMAP: &glib::GStr = glib::gstr!("face-det-lite-out-heatmap");
const FACE_DET_LITE_BOXES: &glib::GStr = glib::gstr!("face-det-lite-out-boxes");
const FACE_DET_LITE_LANDMARKS: &glib::GStr = glib::gstr!("face-det-lite-out-landmarks");

// v2 (ONNX variant)
const FACE_DET_LITE_V2_HEATMAP: &glib::GStr = glib::gstr!("face-det-lite-out-v2-heatmap");
const FACE_DET_LITE_V2_BOXES: &glib::GStr = glib::gstr!("face-det-lite-out-v2-boxes");
const FACE_DET_LITE_V2_LANDMARKS: &glib::GStr = glib::gstr!("face-det-lite-out-v2-landmarks");

const STRIDE: f32 = 8.0;
const NUM_LANDMARKS: usize = 5;
const FACE_CLASS: &str = "face";
const FACE_LANDMARKS_TAG: &str = "face-landmarks";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "facedetlitetensordec",
        gst::DebugColorFlags::empty(),
        Some("FaceDetLite tensor decoder element"),
    )
});

struct Settings {
    confidence_threshold: f32,
    nms_iou_threshold: f32,
    max_detections: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            confidence_threshold: 0.30,
            nms_iou_threshold: 0.50,
            max_detections: 100,
        }
    }
}

#[derive(Default)]
pub struct FaceDetLiteTensorDec {
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for FaceDetLiteTensorDec {
    const NAME: &'static str = "GstFaceDetLiteTensorDec";
    type Type = super::FaceDetLiteTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for FaceDetLiteTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("confidence-threshold")
                    .nick("Confidence Threshold")
                    .blurb("Minimum heatmap peak score to consider a face detection")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().confidence_threshold)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("nms-iou-threshold")
                    .nick("NMS IoU Threshold")
                    .blurb("Maximum intersection-over-union between face bounding boxes to consider them distinct")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(Settings::default().nms_iou_threshold)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("max-detections")
                    .nick("Maximum Detections")
                    .blurb("Maximum number of face detections per frame")
                    .default_value(Settings::default().max_detections)
                    .mutable_playing()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "confidence-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.confidence_threshold = value.get().unwrap();
            }
            "nms-iou-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.nms_iou_threshold = value.get().unwrap();
            }
            "max-detections" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_detections = value.get().unwrap();
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "confidence-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.confidence_threshold.to_value()
            }
            "nms-iou-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.nms_iou_threshold.to_value()
            }
            "max-detections" => {
                let settings = self.settings.lock().unwrap();
                settings.max_detections.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for FaceDetLiteTensorDec {}

impl ElementImpl for FaceDetLiteTensorDec {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "FaceDetLite Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes FaceDetLite model tensors to face bounding boxes and facial landmarks",
                "Olivier Crête <olivier.crete@collabora.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let v1_caps = gst::Caps::builder("video/x-raw")
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            FACE_DET_LITE_OUT,
                            gst::UniqueList::new([
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", FACE_DET_LITE_HEATMAP.as_str())
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            1i32.to_send_value(),
                                        ]),
                                    )
                                    .field("dims-order", "row-major")
                                    .field("type", "float32")
                                    .build()
                                    .to_send_value(),
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", FACE_DET_LITE_BOXES.as_str())
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            4i32.to_send_value(),
                                        ]),
                                    )
                                    .field("dims-order", "row-major")
                                    .field("type", "float32")
                                    .build()
                                    .to_send_value(),
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", FACE_DET_LITE_LANDMARKS.as_str())
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            10i32.to_send_value(),
                                        ]),
                                    )
                                    .field("dims-order", "row-major")
                                    .field("type", "float32")
                                    .build()
                                    .to_send_value(),
                            ]),
                        )
                        .build(),
                )
                .build();

            let v2_caps = gst::Caps::builder("video/x-raw")
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            FACE_DET_LITE_OUT_V2,
                            gst::UniqueList::new([
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", FACE_DET_LITE_V2_HEATMAP.as_str())
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            1i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field("dims-order", "row-major")
                                    .field("type", "float32")
                                    .build()
                                    .to_send_value(),
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", FACE_DET_LITE_V2_BOXES.as_str())
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            4i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field("dims-order", "row-major")
                                    .field("type", "float32")
                                    .build()
                                    .to_send_value(),
                                gst::Caps::builder("tensor/strided")
                                    .field("tensor-id", FACE_DET_LITE_V2_LANDMARKS.as_str())
                                    .field(
                                        "dims",
                                        gst::Array::from_values([
                                            1i32.to_send_value(),
                                            10i32.to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                            gst::IntRange::<i32>::new(1, i32::MAX).to_send_value(),
                                        ]),
                                    )
                                    .field("dims-order", "row-major")
                                    .field("type", "float32")
                                    .build()
                                    .to_send_value(),
                            ]),
                        )
                        .build(),
                )
                .build();

            let mut sink_caps = v1_caps;
            sink_caps.merge(v2_caps);

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("video/x-raw").build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for FaceDetLiteTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let result = with_face_det_tensors(buffer, |heatmap_data, fm_h, fm_w, boxes_data, landmarks_data| {
            let settings = self.settings.lock().unwrap();
            let confidence_threshold = settings.confidence_threshold;
            let nms_iou_threshold = settings.nms_iou_threshold;
            let max_detections = settings.max_detections as usize;
            drop(settings);

            // Step 1: sigmoid-activate the heatmap
            let heatmap: Vec<f32> = heatmap_data.iter().map(|&v| sigmoid(v)).collect();

            // Step 2: 3×3 max-pool NMS — keep only local maxima
            let heatmap_nms = max_pool_nms(&heatmap, fm_h, fm_w);

            // Step 3: collect candidates above threshold
            let mut candidates: Vec<FaceCandidate> = heatmap_nms
                .iter()
                .enumerate()
                .filter(|&(_, &score)| score >= confidence_threshold)
                .map(|(flat_idx, &score)| {
                    let cy = flat_idx / fm_w;
                    let cx = flat_idx % fm_w;
                    decode_candidate(cy, cx, score, boxes_data, landmarks_data, fm_w)
                })
                .collect();

            gst::log!(
                CAT,
                imp = self,
                "Found {} candidates above threshold {:.2}",
                candidates.len(),
                confidence_threshold,
            );

            // Step 4: IoU-based NMS
            candidates.sort_unstable_by(|a, b| b.score.total_cmp(&a.score));
            apply_nms(candidates, max_detections, nms_iou_threshold)
        });

        let detections = match result {
            Some(d) => d,
            None => {
                gst::trace!(CAT, imp = self, "No FaceDetLite tensor meta found");
                return Ok(gst::FlowSuccess::Ok);
            }
        };

        gst::log!(CAT, imp = self, "After NMS: {} detections", detections.len());

        if detections.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        // Step 5: attach analytics metadata
        let face_class = glib::Quark::from_str(FACE_CLASS);
        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buffer);

        for det in &detections {
            let x = det.left as i32;
            let y = det.top as i32;
            let width = (det.right - det.left).max(0.0) as i32;
            let height = (det.bottom - det.top).max(0.0) as i32;

            gst::log!(
                CAT,
                imp = self,
                "Face at ({x}, {y}) {width}×{height} score={:.3}",
                det.score,
            );

            let od_id = match rmeta.add_od_mtd(face_class, x, y, width, height, det.score) {
                Ok(r) => r.id(),
                Err(err) => {
                    gst::warning!(CAT, imp = self, "Failed to add OD metadata: {err}");
                    continue;
                }
            };

            let positions: Vec<gst_analytics::AnalyticsKeypointPosition> = det
                .landmarks
                .iter()
                .map(|(lx, ly)| gst_analytics::AnalyticsKeypointPosition {
                    x: *lx as i32,
                    y: *ly as i32,
                    z: 0,
                    dimension: gst_analytics::AnalyticsKeypointDimensions::_2d,
                })
                .collect();

            let group_id = match rmeta.add_keypoints_group_from_positions(
                FACE_LANDMARKS_TAG,
                &positions,
                None,
                None,
                &[],
            ) {
                Ok(r) => r.id(),
                Err(err) => {
                    gst::warning!(CAT, imp = self, "Failed to add keypoints group: {err}");
                    continue;
                }
            };

            if let Err(err) =
                rmeta.set_relation(gst_analytics::RelTypes::CONTAIN, od_id, group_id)
            {
                gst::warning!(CAT, imp = self, "Failed to set CONTAIN relation: {err}");
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

// ------ decoding helpers ------

struct FaceCandidate {
    score: f32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    landmarks: [(f32, f32); NUM_LANDMARKS],
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// 3×3 max-pool with stride=1 and same (zero-replicated-edge) padding.
/// Returns a flat vec of length fm_h * fm_w; non-maxima are set to zero.
fn max_pool_nms(heatmap: &[f32], fm_h: usize, fm_w: usize) -> Vec<f32> {
    let mut pooled = vec![0.0f32; fm_h * fm_w];
    for r in 0..fm_h {
        for c in 0..fm_w {
            let mut max_val = 0.0f32;
            let r_start = r.saturating_sub(1);
            let r_end = (r + 1).min(fm_h - 1);
            let c_start = c.saturating_sub(1);
            let c_end = (c + 1).min(fm_w - 1);
            for pr in r_start..=r_end {
                for pc in c_start..=c_end {
                    let v = heatmap[pr * fm_w + pc];
                    if v > max_val {
                        max_val = v;
                    }
                }
            }
            let orig = heatmap[r * fm_w + c];
            // keep only if orig is the local maximum
            pooled[r * fm_w + c] = if orig >= max_val { orig } else { 0.0 };
        }
    }
    pooled
}

fn decode_candidate(
    cy: usize,
    cx: usize,
    score: f32,
    boxes: &[f32],
    landmarks: &[f32],
    fm_w: usize,
) -> FaceCandidate {
    let cell = cy * fm_w + cx;
    let b = &boxes[cell * 4..cell * 4 + 4];
    let left = (cx as f32 - b[0]) * STRIDE;
    let top = (cy as f32 - b[1]) * STRIDE;
    let right = (cx as f32 + b[2]) * STRIDE;
    let bottom = (cy as f32 + b[3]) * STRIDE;

    let lm = &landmarks[cell * 10..cell * 10 + 10];
    let mut kpts = [(0.0f32, 0.0f32); NUM_LANDMARKS];
    for k in 0..NUM_LANDMARKS {
        kpts[k] = (
            (lm[k] + cx as f32) * STRIDE,
            (lm[NUM_LANDMARKS + k] + cy as f32) * STRIDE,
        );
    }

    FaceCandidate {
        score,
        left,
        top,
        right,
        bottom,
        landmarks: kpts,
    }
}

fn iou(a: &FaceCandidate, b: &FaceCandidate) -> f32 {
    let ix_min = a.left.max(b.left);
    let iy_min = a.top.max(b.top);
    let ix_max = a.right.min(b.right);
    let iy_max = a.bottom.min(b.bottom);

    let inter_w = (ix_max - ix_min).max(0.0);
    let inter_h = (iy_max - iy_min).max(0.0);
    let inter = inter_w * inter_h;

    if inter == 0.0 {
        return 0.0;
    }

    let area_a = (a.right - a.left).max(0.0) * (a.bottom - a.top).max(0.0);
    let area_b = (b.right - b.left).max(0.0) * (b.bottom - b.top).max(0.0);
    inter / (area_a + area_b - inter)
}

fn apply_nms(
    candidates: Vec<FaceCandidate>,
    max_detections: usize,
    iou_threshold: f32,
) -> Vec<FaceCandidate> {
    let mut selected: Vec<FaceCandidate> = Vec::with_capacity(max_detections.min(candidates.len()));

    'candidate: for candidate in candidates {
        for kept in &selected {
            if iou(&candidate, kept) > iou_threshold {
                continue 'candidate;
            }
        }
        selected.push(candidate);
        if selected.len() >= max_detections {
            break;
        }
    }

    selected
}

/// Transpose an NCHW tensor (channels-first, e.g. shape `1×C×H×W`) into NHWC
/// (channels-last, `1×H×W×C`) so it can be consumed by the shared NHWC decoder.
fn nchw_to_nhwc(src: &[f32], channels: usize, fm_h: usize, fm_w: usize) -> Vec<f32> {
    let plane = fm_h * fm_w;
    let mut out = vec![0.0f32; channels * plane];
    for c in 0..channels {
        for hw in 0..plane {
            out[hw * channels + c] = src[c * plane + hw];
        }
    }
    out
}

fn with_face_det_tensors<F, R>(buffer: &gst::BufferRef, f: F) -> Option<R>
where
    F: FnOnce(&[f32], usize, usize, &[f32], &[f32]) -> R,
{
    let heatmap_quark_v1 = glib::Quark::from_static_str(FACE_DET_LITE_HEATMAP);
    let boxes_quark_v1 = glib::Quark::from_static_str(FACE_DET_LITE_BOXES);
    let landmarks_quark_v1 = glib::Quark::from_static_str(FACE_DET_LITE_LANDMARKS);

    let heatmap_quark_v2 = glib::Quark::from_static_str(FACE_DET_LITE_V2_HEATMAP);
    let boxes_quark_v2 = glib::Quark::from_static_str(FACE_DET_LITE_V2_BOXES);
    let landmarks_quark_v2 = glib::Quark::from_static_str(FACE_DET_LITE_V2_LANDMARKS);

    for meta in buffer.iter_meta::<gst_analytics::TensorMeta>() {
        // Check which variant is present by looking at tensor IDs
        let tensors = meta.as_slice();
        let has_v1 = tensors.iter().any(|t| t.id() == heatmap_quark_v1);
        let has_v2 = tensors.iter().any(|t| t.id() == heatmap_quark_v2);

        if has_v1 {
            let heatmap_tensor = meta.typed_tensor(
                heatmap_quark_v1,
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[1, usize::MAX, usize::MAX, 1],
            );

            if let Some(heatmap_tensor) = heatmap_tensor {
                let fm_h = heatmap_tensor.dims()[1];
                let fm_w = heatmap_tensor.dims()[2];

                let boxes_tensor = meta.typed_tensor(
                    boxes_quark_v1,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[1, fm_h, fm_w, 4],
                )?;

                let landmarks_tensor = meta.typed_tensor(
                    landmarks_quark_v1,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[1, fm_h, fm_w, 10],
                )?;

                let hm_map = heatmap_tensor.data().map_readable().ok()?;
                let hm_data = hm_map.as_slice_of::<f32>().ok()?;

                let bx_map = boxes_tensor.data().map_readable().ok()?;
                let bx_data = bx_map.as_slice_of::<f32>().ok()?;

                let lm_map = landmarks_tensor.data().map_readable().ok()?;
                let lm_data = lm_map.as_slice_of::<f32>().ok()?;

                return Some(f(hm_data, fm_h, fm_w, bx_data, lm_data));
            }
        } else if has_v2 {
            let heatmap_tensor = meta.typed_tensor(
                heatmap_quark_v2,
                gst_analytics::TensorDataType::Float32,
                gst_analytics::TensorDimOrder::RowMajor,
                &[1, 1, usize::MAX, usize::MAX],
            );

            if let Some(heatmap_tensor) = heatmap_tensor {
                let fm_h = heatmap_tensor.dims()[2];
                let fm_w = heatmap_tensor.dims()[3];

                let boxes_tensor = meta.typed_tensor(
                    boxes_quark_v2,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[1, 4, fm_h, fm_w],
                )?;

                let landmarks_tensor = meta.typed_tensor(
                    landmarks_quark_v2,
                    gst_analytics::TensorDataType::Float32,
                    gst_analytics::TensorDimOrder::RowMajor,
                    &[1, 10, fm_h, fm_w],
                )?;

                let hm_map = heatmap_tensor.data().map_readable().ok()?;
                let hm_data = hm_map.as_slice_of::<f32>().ok()?;

                let bx_map = boxes_tensor.data().map_readable().ok()?;
                let bx_data = bx_map.as_slice_of::<f32>().ok()?;

                let lm_map = landmarks_tensor.data().map_readable().ok()?;
                let lm_data = lm_map.as_slice_of::<f32>().ok()?;

                // The v2 ONNX variant produces tensors in NCHW (channels-first)
                // layout. Transpose boxes and landmarks to NHWC so the shared
                // decoder can index them with `cell * C + c`. Heatmap is single
                // channel so the layout is identical either way.
                let bx_nhwc = nchw_to_nhwc(bx_data, 4, fm_h, fm_w);
                let lm_nhwc = nchw_to_nhwc(lm_data, 10, fm_h, fm_w);

                return Some(f(hm_data, fm_h, fm_w, &bx_nhwc, &lm_nhwc));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigmoid_at_zero_is_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn sigmoid_large_positive_approaches_one() {
        assert!((sigmoid(20.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sigmoid_large_negative_approaches_zero() {
        assert!(sigmoid(-20.0) < 1e-6);
    }

    #[test]
    fn max_pool_nms_suppresses_non_maxima() {
        // 3×3 heatmap, centre cell is the maximum
        #[rustfmt::skip]
        let hm = vec![
            0.1, 0.2, 0.1,
            0.2, 0.9, 0.2,
            0.1, 0.2, 0.1,
        ];
        let result = max_pool_nms(&hm, 3, 3);
        // Only the centre peak (index 4) should survive
        assert!(result[4] > 0.0, "centre peak must survive");
        for (i, &v) in result.iter().enumerate() {
            if i != 4 {
                assert_eq!(v, 0.0, "non-maximum at index {i} must be suppressed");
            }
        }
    }

    #[test]
    fn max_pool_nms_preserves_all_equal_peaks() {
        // All cells equal — every cell is its own maximum so none are suppressed
        let hm = vec![0.5f32; 9];
        let result = max_pool_nms(&hm, 3, 3);
        for &v in &result {
            assert!((v - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn decode_box_at_origin() {
        // cy=0, cx=0, offsets all 1.0 → left=-8, top=-8, right=8, bottom=8
        let mut boxes = vec![0.0f32; 4];
        boxes[0] = 1.0; // left offset
        boxes[1] = 1.0; // top offset
        boxes[2] = 1.0; // right offset
        boxes[3] = 1.0; // bottom offset
        let landmarks = vec![0.0f32; 10];
        let c = decode_candidate(0, 0, 0.5, &boxes, &landmarks, 1);
        assert!((c.left - (-8.0)).abs() < 1e-4);
        assert!((c.top - (-8.0)).abs() < 1e-4);
        assert!((c.right - 8.0).abs() < 1e-4);
        assert!((c.bottom - 8.0).abs() < 1e-4);
    }

    #[test]
    fn decode_box_away_from_origin() {
        // cy=2, cx=3, stride=8: cell centre at pixel (24, 16)
        // offsets: left=1, top=1, right=1, bottom=1
        // → left=(3-1)*8=16, top=(2-1)*8=8, right=(3+1)*8=32, bottom=(2+1)*8=24
        let fm_w = 8;
        let cell = 2 * fm_w + 3;
        let mut boxes = vec![0.0f32; fm_w * 4 * 8];
        boxes[cell * 4] = 1.0;
        boxes[cell * 4 + 1] = 1.0;
        boxes[cell * 4 + 2] = 1.0;
        boxes[cell * 4 + 3] = 1.0;
        let landmarks = vec![0.0f32; fm_w * 10 * 8];
        let c = decode_candidate(2, 3, 0.8, &boxes, &landmarks, fm_w);
        assert!((c.left - 16.0).abs() < 1e-4, "left={}", c.left);
        assert!((c.top - 8.0).abs() < 1e-4, "top={}", c.top);
        assert!((c.right - 32.0).abs() < 1e-4, "right={}", c.right);
        assert!((c.bottom - 24.0).abs() < 1e-4, "bottom={}", c.bottom);
    }

    #[test]
    fn decode_landmarks_at_cell() {
        // cy=1, cx=2, fm_w=4
        // landmark x offsets: [0.5, 0.5, 0.5, 0.5, 0.5]
        // landmark y offsets: [0.5, 0.5, 0.5, 0.5, 0.5]
        // expected: x = (0.5 + 2) * 8 = 20, y = (0.5 + 1) * 8 = 12
        let fm_w = 4;
        let cell = 1 * fm_w + 2;
        let mut landmarks = vec![0.0f32; fm_w * 10 * 4];
        for k in 0..10 {
            landmarks[cell * 10 + k] = 0.5;
        }
        let boxes = vec![0.0f32; fm_w * 4 * 4];
        let c = decode_candidate(1, 2, 0.9, &boxes, &landmarks, fm_w);
        for (lx, ly) in &c.landmarks {
            assert!((lx - 20.0).abs() < 1e-3, "lx={lx}");
            assert!((ly - 12.0).abs() < 1e-3, "ly={ly}");
        }
    }

    #[test]
    fn iou_zero_for_non_overlapping() {
        let a = make_candidate(0.9, 0.0, 0.0, 10.0, 10.0);
        let b = make_candidate(0.8, 20.0, 20.0, 30.0, 30.0);
        assert_eq!(iou(&a, &b), 0.0);
    }

    #[test]
    fn iou_one_for_identical_boxes() {
        let a = make_candidate(0.9, 10.0, 10.0, 20.0, 20.0);
        let b = make_candidate(0.8, 10.0, 10.0, 20.0, 20.0);
        assert!((iou(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn nms_removes_overlapping_detection() {
        let high = make_candidate(0.9, 0.0, 0.0, 100.0, 100.0);
        let low = make_candidate(0.5, 0.0, 0.0, 100.0, 100.0);
        let result = apply_nms(vec![high, low], 10, 0.5);
        assert_eq!(result.len(), 1);
        assert!((result[0].score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn nms_keeps_non_overlapping_detections() {
        let a = make_candidate(0.9, 0.0, 0.0, 50.0, 50.0);
        let b = make_candidate(0.8, 200.0, 200.0, 250.0, 250.0);
        let result = apply_nms(vec![a, b], 10, 0.5);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn nms_respects_max_detections() {
        let candidates: Vec<FaceCandidate> = (0..20)
            .map(|i| make_candidate(0.9 - i as f32 * 0.01, i as f32 * 200.0, 0.0, i as f32 * 200.0 + 100.0, 100.0))
            .collect();
        let result = apply_nms(candidates, 5, 0.5);
        assert_eq!(result.len(), 5);
    }

    fn make_candidate(score: f32, left: f32, top: f32, right: f32, bottom: f32) -> FaceCandidate {
        FaceCandidate {
            score,
            left,
            top,
            right,
            bottom,
            landmarks: [(0.0, 0.0); NUM_LANDMARKS],
        }
    }
}
