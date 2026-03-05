// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! # handlandmarktensordec
//!
//! A GStreamer element that attaches hand keypoint tensors to video buffers for downstream processing.
//!
//! This element extracts hand landmark keypoints from hand landmark tensors (produced by ONNX
//! hand landmark inference models) and attaches them as tensor metadata to the buffer. This allows
//! downstream elements to perform gesture recognition, hand pose analysis, or other ML tasks that
//! require access to the raw keypoint coordinates.
//!
//! The element is designed to work with hand landmark models that output tensors with:
//! - `hand_landmarks`: 2D keypoints for each hand (21 points × 2 coordinates per point)
//! - `hand_score`: Confidence score per hand (optional)
//! - `hand_rotation`: Hand rotation angle per hand (optional; computed from landmarks if not provided)
//!
//! ## Properties
//! - `confidence-threshold` (f32, 0.0-1.0, default: 0.5): Minimum confidence to consider a hand
//! - `max-hands` (u32, 1-10, default: 2): Maximum number of hands to process
//!
//! ## Example Pipelines
//!
//! Gesture recognition pipeline:
//! ```text
//! gst-launch-1.0 \
//!   v4l2src \
//!   ! videoconvert ! videoscale \
//!   ! onnxinference model-file=hand_landmark_model.onnx \
//!   ! handlandmarktensordec confidence-threshold=0.5 max-hands=2 \
//!   ! your_gesture_recognition_element \
//!   ! autovideosink
//! ```
//!
//! Combined detection and landmark analysis:
//! ```text
//! gst-launch-1.0 \
//!   v4l2src \
//!   ! videoconvert ! videoscale \
//!   ! onnxinference model-file=hand_landmark_model.onnx \
//!   ! handdetectiontensordec confidence-threshold=0.7 \
//!   ! handlandmarktensordec confidence-threshold=0.5 \
//!   ! objectdetectionoverlay \
//!   ! videoconvert ! autovideosink
//! ```

use gst::glib;
use gst::prelude::*;
use gst::subclass::ElementMetadata;
use gst::subclass::prelude::*;
use gst_analytics::prelude::*;
use gst_base::subclass::base_transform::BaseTransformImpl;
use gst_video::VideoInfo;
use std::sync::{LazyLock, Mutex};

use super::super::helper::{
    bbox_iou, extract_f32_tensor, oriented_od_params_from_bbox_and_rotation,
};

const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.5;
const DEFAULT_MAX_HANDS: u32 = 2;
const DEFAULT_NMS_IOU_THRESHOLD: f32 = 0.2;
const DEFAULT_ATTACH_BOUNDING_BOX: bool = true;
const HAND_CLASS_LABEL: &str = "hand";
const HAND_LANDMARKS_TENSOR_ID: &str = "hand_landmarks";
const HAND_SCORE_TENSOR_ID: &str = "hand_score";
const HAND_KEYPOINT_COUNT: usize = 21;
const HAND_BBOX_PADDING: f32 = 0.15;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "handlandmarktensordec",
        gst::DebugColorFlags::empty(),
        Some("Hand landmark tensor decoder element"),
    )
});

#[derive(Clone, Debug)]
struct HandData {
    confidence: f32,
    rotation: f32,
    bbox: (f32, f32, f32, f32),
    landmarks: Vec<f32>,
    stride: usize,
}

fn decode_landmark_hands(data: &[f32], dims: &[usize]) -> Option<(Vec<Vec<f32>>, usize)> {
    match dims {
        [batch, hands, keypoints, stride] if *keypoints == HAND_KEYPOINT_COUNT && *stride >= 2 => {
            let hand_len = HAND_KEYPOINT_COUNT * *stride;
            let total_hands = *batch * *hands;
            if data.len() < total_hands * hand_len {
                return None;
            }

            Some((
                data.chunks_exact(hand_len)
                    .take(total_hands)
                    .map(|chunk| chunk.to_vec())
                    .collect(),
                *stride,
            ))
        }
        [hands, keypoints, stride] if *keypoints == HAND_KEYPOINT_COUNT && *stride >= 2 => {
            let hand_len = HAND_KEYPOINT_COUNT * *stride;
            if data.len() < *hands * hand_len {
                return None;
            }

            Some((
                data.chunks_exact(hand_len)
                    .take(*hands)
                    .map(|chunk| chunk.to_vec())
                    .collect(),
                *stride,
            ))
        }
        [keypoints, stride, last]
            if *keypoints == HAND_KEYPOINT_COUNT && *stride >= 2 && *last == 1 =>
        {
            let hand_len = HAND_KEYPOINT_COUNT * *stride;
            if data.len() < hand_len {
                return None;
            }

            Some((vec![data[..hand_len].to_vec()], *stride))
        }
        [hands, flattened] if *flattened % HAND_KEYPOINT_COUNT == 0 => {
            let stride = *flattened / HAND_KEYPOINT_COUNT;
            if stride < 2 || data.len() < *hands * *flattened {
                return None;
            }

            Some((
                data.chunks_exact(*flattened)
                    .take(*hands)
                    .map(|chunk| chunk.to_vec())
                    .collect(),
                stride,
            ))
        }
        [keypoints, stride] if *keypoints == HAND_KEYPOINT_COUNT && *stride >= 2 => {
            let hand_len = HAND_KEYPOINT_COUNT * *stride;
            if data.len() < hand_len {
                return None;
            }

            Some((vec![data[..hand_len].to_vec()], *stride))
        }
        [flattened] if *flattened % HAND_KEYPOINT_COUNT == 0 => {
            let stride = *flattened / HAND_KEYPOINT_COUNT;
            if stride < 2 || data.len() < *flattened {
                return None;
            }

            Some((vec![data[..*flattened].to_vec()], stride))
        }
        _ => None,
    }
}

fn normalize_landmark_axis(value: f32, size: i32) -> f32 {
    if size > 0 && (0.0..=1.0).contains(&value) {
        value * size as f32
    } else {
        value
    }
}

fn compute_rotation_from_landmarks(landmarks: &[f32], stride: usize) -> Option<f32> {
    if stride < 2 || landmarks.len() < HAND_KEYPOINT_COUNT * stride {
        return None;
    }

    let wrist_x = landmarks[0];
    let wrist_y = landmarks[1];
    let index_base_x = landmarks[5 * stride];
    let index_base_y = landmarks[5 * stride + 1];
    let middle_base_x = landmarks[9 * stride];
    let middle_base_y = landmarks[9 * stride + 1];
    let ring_base_x = landmarks[13 * stride];
    let ring_base_y = landmarks[13 * stride + 1];
    let pinky_base_x = landmarks[17 * stride];
    let pinky_base_y = landmarks[17 * stride + 1];

    let palm_x = (wrist_x + index_base_x + middle_base_x + ring_base_x + pinky_base_x) / 5.0;
    let palm_y = (wrist_y + index_base_y + middle_base_y + ring_base_y + pinky_base_y) / 5.0;

    let middle_tip_x = landmarks[12 * stride];
    let middle_tip_y = landmarks[12 * stride + 1];

    Some(std::f32::consts::FRAC_PI_2 + (middle_tip_y - palm_y).atan2(middle_tip_x - palm_x))
}

fn compute_bbox_from_landmarks(
    landmarks: &[f32],
    stride: usize,
    video_size: Option<(i32, i32)>,
) -> Option<(f32, f32, f32, f32)> {
    if stride < 2 || landmarks.len() < HAND_KEYPOINT_COUNT * stride {
        return None;
    }

    let (frame_width, frame_height) = video_size.unwrap_or((0, 0));
    let mut xs = Vec::with_capacity(HAND_KEYPOINT_COUNT);
    let mut ys = Vec::with_capacity(HAND_KEYPOINT_COUNT);

    for point in landmarks.chunks_exact(stride).take(HAND_KEYPOINT_COUNT) {
        let x = point[0];
        let y = point[1];
        if !x.is_finite() || !y.is_finite() {
            continue;
        }

        xs.push(normalize_landmark_axis(x, frame_width));
        ys.push(normalize_landmark_axis(y, frame_height));
    }

    if xs.is_empty() || ys.is_empty() {
        return None;
    }

    let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
    let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
    let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);

    let width = max_x - min_x;
    let height = max_y - min_y;

    if width <= 0.0 || height <= 0.0 {
        return None;
    }

    Some((
        min_x - width * HAND_BBOX_PADDING,
        min_y - height * HAND_BBOX_PADDING,
        max_x + width * HAND_BBOX_PADDING,
        max_y + height * HAND_BBOX_PADDING,
    ))
}

fn flatten_optional_hand_values(
    buffer: &gst::BufferRef,
    tensor_id: &'static str,
) -> Option<Vec<f32>> {
    let tensor_id = glib::Quark::from_str(tensor_id);
    extract_f32_tensor(buffer, tensor_id).map(|(data, _dims)| data)
}

fn extract_hands(
    buffer: &gst::BufferRef,
    max_hands: usize,
    confidence_threshold: f32,
    nms_iou_threshold: f32,
    video_size: Option<(i32, i32)>,
) -> Result<Vec<HandData>, gst::FlowError> {
    let landmarks_id = glib::Quark::from_str(HAND_LANDMARKS_TENSOR_ID);
    let Some((landmark_data, landmark_dims)) = extract_f32_tensor(buffer, landmarks_id) else {
        gst::debug!(CAT, "No hand landmarks tensor found");
        return Ok(Vec::new());
    };

    let Some((hands_landmarks, stride)) = decode_landmark_hands(&landmark_data, &landmark_dims)
    else {
        gst::warning!(
            CAT,
            "Unsupported landmarks tensor dims: {:?}",
            landmark_dims
        );
        return Ok(Vec::new());
    };

    let hand_scores = flatten_optional_hand_values(buffer, HAND_SCORE_TENSOR_ID);

    let mut hands = Vec::new();

    for (index, landmarks) in hands_landmarks.into_iter().enumerate() {
        let confidence = hand_scores
            .as_ref()
            .and_then(|scores| scores.get(index).copied())
            .unwrap_or(1.0);
        if confidence < confidence_threshold {
            continue;
        }

        let Some(bbox) = compute_bbox_from_landmarks(&landmarks, stride, video_size) else {
            continue;
        };

        let rotation = compute_rotation_from_landmarks(&landmarks, stride).unwrap_or(0.0);

        hands.push(HandData {
            confidence,
            rotation,
            bbox,
            landmarks: landmarks.clone(),
            stride,
        });
    }

    hands.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));

    let mut selected: Vec<HandData> = Vec::with_capacity(max_hands.min(hands.len()));
    let iou_threshold = nms_iou_threshold.clamp(0.0, 1.0);

    'candidate: for hand in hands {
        for kept in &selected {
            if bbox_iou(hand.bbox, kept.bbox) > iou_threshold {
                continue 'candidate;
            }
        }

        selected.push(hand);
        if selected.len() >= max_hands {
            break;
        }
    }

    Ok(selected)
}

fn extract_keypoint_confidence(hand: &HandData, keypoint_idx: usize) -> Option<f32> {
    if hand.stride < 3 || keypoint_idx >= HAND_KEYPOINT_COUNT {
        return None;
    }
    let confidence_idx = keypoint_idx * hand.stride + 2;
    hand.landmarks.get(confidence_idx).copied()
}

fn attach_keypoint_metadata(
    rmeta: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    hand: &HandData,
    video_size: Option<(i32, i32)>,
) -> Result<(), String> {
    if hand.stride < 2 || hand.landmarks.len() < HAND_KEYPOINT_COUNT * hand.stride {
        return Err("Invalid landmarks data".to_string());
    }

    let (frame_width, frame_height) = video_size.unwrap_or((0, 0));

    for (keypoint_idx, point) in hand
        .landmarks
        .chunks_exact(hand.stride)
        .take(HAND_KEYPOINT_COUNT)
        .enumerate()
    {
        let x = point[0];
        let y = point[1];

        if !x.is_finite() || !y.is_finite() {
            continue;
        }

        let px = normalize_landmark_axis(x, frame_width) as i32;
        let py = normalize_landmark_axis(y, frame_height) as i32;
        let keypoint_confidence = extract_keypoint_confidence(hand, keypoint_idx);

        // Determine visibility based on per-keypoint confidence if available
        let visibility = if let Some(kp_conf) = keypoint_confidence {
            if kp_conf > 0.5 {
                gst_analytics::AnalyticsKeypointVisibility::VISIBLE
            } else {
                gst_analytics::AnalyticsKeypointVisibility::OCCLUDED
            }
        } else {
            gst_analytics::AnalyticsKeypointVisibility::VISIBLE
        };

        // Keep keypoint confidence when available; otherwise fall back to hand-level confidence.
        let confidence = keypoint_confidence.unwrap_or(hand.confidence);

        rmeta
            .add_keypoint_mtd(
                gst_analytics::AnalyticsKeypointDimensions::_2d,
                px,
                py,
                0,
                visibility,
                confidence,
            )
            .map_err(|e| format!("Failed to add keypoint {}: {}", keypoint_idx, e))?;
    }

    Ok(())
}

fn hand_bbox_to_oriented_od_params(
    hand: &HandData,
    video_size: Option<(i32, i32)>,
) -> Option<(i32, i32, i32, i32, f32)> {
    oriented_od_params_from_bbox_and_rotation(hand.bbox, hand.rotation, video_size)
}

#[derive(Clone)]
struct Settings {
    confidence_threshold: f32,
    max_hands: u32,
    nms_iou_threshold: f32,
    attach_bounding_box: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            confidence_threshold: DEFAULT_CONFIDENCE_THRESHOLD,
            max_hands: DEFAULT_MAX_HANDS,
            nms_iou_threshold: DEFAULT_NMS_IOU_THRESHOLD,
            attach_bounding_box: DEFAULT_ATTACH_BOUNDING_BOX,
        }
    }
}

#[derive(Default)]
pub struct HandLandmarkTensorDec {
    settings: Mutex<Settings>,
    video_info: Mutex<Option<VideoInfo>>,
}

#[glib::object_subclass]
impl ObjectSubclass for HandLandmarkTensorDec {
    const NAME: &'static str = "GstHandLandmarkTensorDec";
    type Type = super::HandLandmarkTensorDec;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for HandLandmarkTensorDec {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecFloat::builder("confidence-threshold")
                    .nick("Confidence Threshold")
                    .blurb("Confidence threshold for hand detection")
                    .default_value(DEFAULT_CONFIDENCE_THRESHOLD)
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("max-hands")
                    .nick("Max Hands")
                    .blurb("Maximum number of hands to track")
                    .default_value(DEFAULT_MAX_HANDS)
                    .minimum(1)
                    .maximum(10)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("nms-iou-threshold")
                    .nick("NMS IoU Threshold")
                    .blurb("IoU threshold for non-maximum suppression on hand detections")
                    .default_value(DEFAULT_NMS_IOU_THRESHOLD)
                    .minimum(0.0)
                    .maximum(1.0)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("attach-bounding-box")
                    .nick("Attach Bounding Box")
                    .blurb("Whether to attach oriented bounding box metadata for each hand")
                    .default_value(DEFAULT_ATTACH_BOUNDING_BOX)
                    .mutable_playing()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "confidence-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.confidence_threshold = value.get().expect("type checked upstream");
            }
            "max-hands" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_hands = value.get().expect("type checked upstream");
            }
            "nms-iou-threshold" => {
                let mut settings = self.settings.lock().unwrap();
                settings.nms_iou_threshold = value.get().expect("type checked upstream");
            }
            "attach-bounding-box" => {
                let mut settings = self.settings.lock().unwrap();
                settings.attach_bounding_box = value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "confidence-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.confidence_threshold.to_value()
            }
            "max-hands" => {
                let settings = self.settings.lock().unwrap();
                settings.max_hands.to_value()
            }
            "nms-iou-threshold" => {
                let settings = self.settings.lock().unwrap();
                settings.nms_iou_threshold.to_value()
            }
            "attach-bounding-box" => {
                let settings = self.settings.lock().unwrap();
                settings.attach_bounding_box.to_value()
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for HandLandmarkTensorDec {}

impl ElementImpl for HandLandmarkTensorDec {
    fn metadata() -> Option<&'static ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<ElementMetadata> = LazyLock::new(|| {
            ElementMetadata::new(
                "Hand Landmark Tensor Decoder",
                "Tensordecoder/Video",
                "Decodes hand landmark tensors and attaches keypoint metadata",
                "Jeremy Whiting <jeremy.whiting@collabora.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .field(
                    "tensors",
                    gst::Structure::builder("tensorgroups")
                        .field(
                            HAND_LANDMARKS_TENSOR_ID,
                            gst::UniqueList::new([gst::Caps::builder("tensor/strided")
                                .field("tensor-id", HAND_LANDMARKS_TENSOR_ID)
                                .field(
                                    "dims",
                                    gst::Array::from_values([
                                        gst::IntRange::<i32>::new(0, i32::MAX).to_send_value(),
                                        (HAND_KEYPOINT_COUNT as i32 * 3).to_send_value(),
                                    ]),
                                )
                                .field("dims-order", "row-major")
                                .field("type", "float32")
                                .build()]),
                        )
                        .build(),
                )
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst_video::VideoCapsBuilder::new().build(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for HandLandmarkTensorDec {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn set_caps(&self, incaps: &gst::Caps, _outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Invalid caps {incaps:?}"))?;
        *self.video_info.lock().unwrap() = Some(info);
        Ok(())
    }

    fn transform_ip(&self, buf: &mut gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let (max_hands, confidence_threshold, nms_iou_threshold, attach_bounding_box) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.max_hands as usize,
                settings.confidence_threshold,
                settings.nms_iou_threshold,
                settings.attach_bounding_box,
            )
        };

        let video_size = self
            .video_info
            .lock()
            .unwrap()
            .as_ref()
            .map(|info| (info.width() as i32, info.height() as i32));

        let hands = extract_hands(
            buf,
            max_hands,
            confidence_threshold,
            nms_iou_threshold,
            video_size,
        )?;

        gst::debug!(CAT, "Extracted {} hands", hands.len());

        if hands.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut rmeta = gst_analytics::AnalyticsRelationMeta::add(buf);
        let class = glib::Quark::from_str(HAND_CLASS_LABEL);

        for hand in &hands {
            // Attach bounding box as oriented object detection metadata if enabled
            if attach_bounding_box {
                let Some((x, y, width, height, rotation_for_od)) =
                    hand_bbox_to_oriented_od_params(hand, video_size)
                else {
                    gst::debug!(CAT, "Skipping invalid/out-of-frame hand bbox");
                    continue;
                };

                if let Err(err) = rmeta.add_oriented_od_mtd(
                    class,
                    x,
                    y,
                    width,
                    height,
                    rotation_for_od,
                    hand.confidence,
                ) {
                    gst::warning!(CAT, "Failed to add oriented OD metadata: {}", err);
                }
            }

            // Attach individual keypoint metadata with visibility flags
            if let Err(err) = attach_keypoint_metadata(&mut rmeta, hand, video_size) {
                gst::debug!(CAT, "Failed to attach keypoint metadata: {}", err);
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landmark_rotation_aligns_with_hand_axis() {
        let mut landmarks = vec![0.0f32; HAND_KEYPOINT_COUNT * 2];

        landmarks[0] = 0.0;
        landmarks[1] = 0.0;
        landmarks[5 * 2] = 0.0;
        landmarks[5 * 2 + 1] = 0.0;
        landmarks[9 * 2] = 0.0;
        landmarks[9 * 2 + 1] = 0.0;
        landmarks[13 * 2] = 0.0;
        landmarks[13 * 2 + 1] = 0.0;
        landmarks[17 * 2] = 0.0;
        landmarks[17 * 2 + 1] = 0.0;

        landmarks[12 * 2] = 1.0;
        landmarks[12 * 2 + 1] = 0.0;

        let rotation = compute_rotation_from_landmarks(&landmarks, 2).unwrap();
        let rotation_for_od = rotation - std::f32::consts::FRAC_PI_2;

        assert!(rotation_for_od.abs() < 1e-6);
    }
}
