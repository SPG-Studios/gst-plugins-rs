// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Deferred overlay-label intent (deferred label rendering).
//!
//! Labels are the only *movable* overlay content, so to relocate them globally
//! by priority an upstream element can defer them: instead of placing and
//! rendering a label itself, it emits a [`LabelIntent`] onto the buffer via the
//! [`OVERLAY_LABELS_META`] custom meta. A downstream compositor reads every
//! element's intents, places them all against each other and the anchored
//! content claimed via [`crate::coordination`], and renders them once.
//!
//! Anchored content (boxes, keypoints, masks) is *not* deferred — it is rendered
//! in place and published as a claim (see [`crate::coordination`]); the
//! compositor only needs those claims to know what to steer labels around.
//!
//! Like the claimed-regions meta this is an in-process prototype carried in the
//! negotiated frame's pixel coordinate space (no scale-aware transform yet), so
//! coordinating elements must share one coordinate space.

use gst::meta::CustomMeta;

use crate::geometry::Rect;

/// Well-known name of the custom buffer meta carrying deferred label intents.
/// Registered once at plugin init via [`register`].
pub const OVERLAY_LABELS_META: &str = "GstAnalyticsOverlayLabels";

/// How a compositor regenerates fallback candidate positions for a deferred
/// label, mirroring how the producing element would have placed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// Anchored to a bounding box (uses `placement::box`-style candidates).
    Box,
    /// Anchored to a point feature such as a keypoint (uses point candidates).
    Point,
}

impl CandidateKind {
    fn as_i32(self) -> i32 {
        match self {
            CandidateKind::Box => 0,
            CandidateKind::Point => 1,
        }
    }

    fn from_i32(value: i32) -> Self {
        match value {
            1 => CandidateKind::Point,
            _ => CandidateKind::Box,
        }
    }
}

/// A label an element deferred for a downstream compositor to place and render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelIntent {
    /// Text to draw.
    pub text: String,
    /// Text colour (ARGB).
    pub color: u32,
    /// The labelled feature's rect (a zero-sized rect for a point feature).
    /// Used to regenerate fallback candidates and to draw a leader line when the
    /// label ends up displaced.
    pub anchor: Rect,
    /// The element's preferred (default) label rect; also gives the label size.
    pub preferred: Rect,
    /// Which candidate generator to use when the preferred rect is taken.
    pub kind: CandidateKind,
    /// Importance, for global ordering and avoidance (see [`crate::coordination`]).
    pub priority: i32,
    /// Identifier of the producing element, for debugging.
    pub owner: String,
}

/// Number of `i32` values packed per label in the meta's `coords` array, in
/// order: anchor x, y, w, h, preferred x, y, w, h, color, kind, priority.
const COORDS_PER_LABEL: usize = 11;

/// Register the overlay-labels meta. Idempotent; call once at plugin init.
pub fn register() {
    if CustomMeta::is_registered(OVERLAY_LABELS_META) {
        return;
    }

    // Carry intents verbatim across buffer copies (e.g. a `videoconvert` between
    // an overlay element and the compositor). Coordinates are not scaled — see
    // the module note.
    CustomMeta::register_with_transform(OVERLAY_LABELS_META, &[], |dest, meta, _src, _type| {
        let labels = decode(meta.structure());
        if let Ok(mut dest_meta) = CustomMeta::add(dest, OVERLAY_LABELS_META) {
            encode(dest_meta.mut_structure(), &labels);
        }
        true
    });
}

/// Append deferred label intents to `buffer`, merging with any already present
/// (so a chain of deferring elements all reach the compositor).
pub fn add_label_intents(buffer: &mut gst::BufferRef, labels: &[LabelIntent]) {
    if labels.is_empty() {
        return;
    }

    let mut all = label_intents(buffer);
    all.extend_from_slice(labels);

    let exists = CustomMeta::from_buffer(buffer, OVERLAY_LABELS_META).is_ok();
    let meta = if exists {
        CustomMeta::from_mut_buffer(buffer, OVERLAY_LABELS_META)
    } else {
        CustomMeta::add(buffer, OVERLAY_LABELS_META)
    };

    // `Err` means the meta type is not registered (see `register`); skip rather
    // than panic so a misconfigured pipeline degrades gracefully.
    if let Ok(mut meta) = meta {
        encode(meta.mut_structure(), &all);
    }
}

/// Read every deferred label intent currently attached to `buffer`.
pub fn label_intents(buffer: &gst::BufferRef) -> Vec<LabelIntent> {
    match CustomMeta::from_buffer(buffer, OVERLAY_LABELS_META) {
        Ok(meta) => decode(meta.structure()),
        Err(_) => Vec::new(),
    }
}

// Stored as parallel arrays in the meta's `gst::Structure`: `coords`
// ([`COORDS_PER_LABEL`] i32 per label) plus `texts` and `owners` (one string per
// label each). Homogeneous arrays keep the encoding interoperable from C.
fn encode(structure: &mut gst::StructureRef, labels: &[LabelIntent]) {
    let mut coords: Vec<i32> = Vec::with_capacity(labels.len() * COORDS_PER_LABEL);
    let mut texts: Vec<String> = Vec::with_capacity(labels.len());
    let mut owners: Vec<String> = Vec::with_capacity(labels.len());
    for label in labels {
        coords.extend_from_slice(&[
            label.anchor.left,
            label.anchor.top,
            label.anchor.width(),
            label.anchor.height(),
            label.preferred.left,
            label.preferred.top,
            label.preferred.width(),
            label.preferred.height(),
            label.color as i32,
            label.kind.as_i32(),
            label.priority,
        ]);
        texts.push(label.text.clone());
        owners.push(label.owner.clone());
    }
    structure.set("coords", gst::Array::new(coords));
    structure.set("texts", gst::Array::new(texts));
    structure.set("owners", gst::Array::new(owners));
}

fn decode(structure: &gst::StructureRef) -> Vec<LabelIntent> {
    let Ok(coords) = structure.get::<gst::Array>("coords") else {
        return Vec::new();
    };
    let texts = structure.get::<gst::Array>("texts").ok();
    let owners = structure.get::<gst::Array>("owners").ok();
    let coords = coords.as_slice();
    let texts = texts.as_ref().map(gst::Array::as_slice).unwrap_or(&[]);
    let owners = owners.as_ref().map(gst::Array::as_slice).unwrap_or(&[]);

    coords
        .chunks_exact(COORDS_PER_LABEL)
        .enumerate()
        .map(|(index, chunk)| {
            let value = |i: usize| chunk[i].get::<i32>().unwrap_or(0);
            let string = |arr: &[gst::glib::SendValue]| {
                arr.get(index)
                    .and_then(|v| v.get::<String>().ok())
                    .unwrap_or_default()
            };
            LabelIntent {
                anchor: Rect::from_xywh(value(0), value(1), value(2), value(3)),
                preferred: Rect::from_xywh(value(4), value(5), value(6), value(7)),
                color: value(8) as u32,
                kind: CandidateKind::from_i32(value(9)),
                priority: value(10),
                text: string(texts),
                owner: string(owners),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            register();
        });
    }

    fn sample() -> Vec<LabelIntent> {
        vec![
            LabelIntent {
                text: "person (c=0.90)".to_string(),
                color: 0xFFFF_FFFF,
                anchor: Rect::from_xywh(10, 20, 40, 30),
                preferred: Rect::from_xywh(10, 8, 80, 12),
                kind: CandidateKind::Box,
                priority: 5,
                owner: "odoverlay".to_string(),
            },
            LabelIntent {
                text: "0.88".to_string(),
                color: 0xFF00_FF00,
                anchor: Rect::from_xywh(100, 100, 0, 0),
                preferred: Rect::from_xywh(96, 84, 32, 12),
                kind: CandidateKind::Point,
                priority: 9,
                owner: "keypointsoverlay".to_string(),
            },
        ]
    }

    #[test]
    fn round_trip_preserves_label_intents() {
        init();

        let input = sample();
        let mut buffer = gst::Buffer::new();
        add_label_intents(buffer.make_mut(), &input);

        assert_eq!(label_intents(buffer.as_ref()), input);
    }

    #[test]
    fn add_label_intents_merges_successive_producers() {
        init();

        let input = sample();
        let mut buffer = gst::Buffer::new();
        add_label_intents(buffer.make_mut(), &input[..1]);
        add_label_intents(buffer.make_mut(), &input[1..]);

        assert_eq!(label_intents(buffer.as_ref()), input);
    }

    #[test]
    fn no_meta_reads_as_empty() {
        init();
        let buffer = gst::Buffer::new();
        assert!(label_intents(buffer.as_ref()).is_empty());
    }
}
