// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Candidate-based label placement shared by the overlay elements.
//!
//! A label is placed by trying its preferred (default) position first, then an
//! ordered list of fallback candidates. The first position whose region is free
//! wins. When every candidate collides with an already-reserved region the label
//! is force-placed at the candidate with the *least* overlap. Any time the label
//! ends up away from its default position it is reported as *displaced*, so the
//! caller can draw a leader line back to the labelled feature.

use crate::coordination::{ClaimedRegion, seed_registry_from_regions};
use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::overlay_intent::{CandidateKind, LabelIntent};
use crate::render::{DrawCommand, LABEL_LAYOUT_GAP, LABEL_LAYOUT_HEIGHT, LEADER_LINE_WIDTH};

/// Offset of the near (primary) ring of candidates from the feature, in pixels.
const CANDIDATE_GAP: i32 = 4;
/// Offset of the far (extended) ring of candidates from the feature, in pixels.
const CANDIDATE_EXT: i32 = 12;
/// Offset of the far (extended) ring of box-label candidates, on top of
/// [`LABEL_LAYOUT_GAP`].
const BOX_CANDIDATE_EXT: i32 = 12;

/// Outcome of [`place_label`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelPlacement {
    /// Region the label was placed in.
    pub rect: Rect,
    /// `true` when the label could not be placed at its default position and
    /// was moved to a fallback (or force-placed at the least-overlapping
    /// candidate). Callers should draw a leader line to the labelled feature.
    pub displaced: bool,
}

/// Place a label using a candidate-based strategy. The positions tried are the
/// `default` followed by `candidates` in order.
///
/// 1. Among positions free of hard regions (boxes, other labels, hard claims),
///    pick the one with the least soft-[`Avoid`](crate::geometry::RegionPriority::Avoid)
///    overlap — a segmentation mask or an outline-box claim. The default wins
///    ties, so a clear default is never moved; a label moves only to dodge a mask
///    when a cleaner spot exists, and sits on the least-covered mask when every
///    hard-free spot overlaps one.
/// 2. If no position is free of hard regions, force-place at the candidate with
///    the least hard overlap.
///
/// Any position lying entirely outside the frame is skipped. Ties are broken by
/// order, so placement is deterministic. A position is *displaced* (callers draw
/// a leader line) whenever it is not the default. Returns `None` when neither the
/// default nor any candidate intersects the frame.
pub fn place_label(
    registry: &mut OccupiedRegionRegistry,
    default: Rect,
    candidates: &[Rect],
) -> Option<LabelPlacement> {
    // Phase 1: among positions free of hard regions, pick the least soft-Avoid
    // (segmentation mask) overlap. The default is considered first and wins ties.
    let positions =
        std::iter::once((default, false)).chain(candidates.iter().map(|&rect| (rect, true)));
    let mut best_clear: Option<(i64, Rect, bool)> = None;
    for (rect, displaced) in positions {
        let Some(hard_overlap) = registry.label_overlap_area(rect) else {
            continue; // entirely outside the frame
        };
        if hard_overlap != 0 {
            continue; // not free of hard regions
        }
        let avoid_overlap = registry.avoid_overlap_area(rect).unwrap_or(0);
        // Strict `<` keeps the earliest position (the default) on ties.
        let better = match best_clear {
            Some((best_avoid, _, _)) => avoid_overlap < best_avoid,
            None => true,
        };
        if better {
            best_clear = Some((avoid_overlap, rect, displaced));
        }
    }
    if let Some((_, rect, displaced)) = best_clear {
        // The position is free of hard regions, so this reservation succeeds.
        registry.reserve_label(rect);
        return Some(LabelPlacement { rect, displaced });
    }

    // Phase 2: every position overlaps a hard region. Force-place at the
    // candidate with the least hard overlap (the default is excluded — it is the
    // position we were trying to move away from).
    let mut best_forced: Option<(i64, usize)> = None;
    for (index, &candidate) in candidates.iter().enumerate() {
        let Some(overlap) = registry.label_overlap_area(candidate) else {
            continue; // entirely outside the frame
        };

        // Strict `<` (i.e. skip on `>=`) keeps the earliest candidate on ties.
        match best_forced {
            Some((best_overlap, _)) if overlap >= best_overlap => {}
            _ => best_forced = Some((overlap, index)),
        }
    }

    let (_, index) = best_forced?;
    let candidate = candidates[index];
    registry.force_reserve_label(candidate);
    Some(LabelPlacement {
        rect: candidate,
        displaced: true,
    })
}

/// Generate fallback candidate regions of size `label_w` x `label_h` around a
/// point feature at (`anchor_x`, `anchor_y`).
///
/// Candidates are ordered from nearest to farthest: a near ring (gap) then a far
/// ring (extended), each visiting above, below, right, left, then the four
/// diagonals. This ordering makes placement deterministic and keeps labels close
/// to their feature when possible.
pub fn point_label_candidates(
    anchor_x: i32,
    anchor_y: i32,
    label_w: i32,
    label_h: i32,
) -> Vec<Rect> {
    let half_w = label_w / 2;
    let half_h = label_h / 2;
    let rect_at = |x: i32, y: i32| Rect::from_xywh(x, y, label_w, label_h);

    [CANDIDATE_GAP, CANDIDATE_EXT]
        .into_iter()
        .flat_map(|off| {
            [
                rect_at(anchor_x - half_w, anchor_y - label_h - off), // above
                rect_at(anchor_x - half_w, anchor_y + off),           // below
                rect_at(anchor_x + off, anchor_y - half_h),           // right
                rect_at(anchor_x - label_w - off, anchor_y - half_h), // left
                rect_at(anchor_x + off, anchor_y - label_h - off),    // above-right
                rect_at(anchor_x - label_w - off, anchor_y - label_h - off), // above-left
                rect_at(anchor_x + off, anchor_y + off),              // below-right
                rect_at(anchor_x - label_w - off, anchor_y + off),    // below-left
            ]
        })
        .collect()
}

/// Fallback label regions around a bounding box `bbox`, ordered near ring then
/// far ring (above, below, right, left in each). Box labels are drawn at the
/// bottom-left of their rectangle, so the baseline `y` maps to the rectangle
/// bottom. Shared by the object-detection overlay and the compositor.
pub fn box_label_candidates(bbox: Rect, label_w: i32) -> Vec<Rect> {
    let h = LABEL_LAYOUT_HEIGHT;
    let rect_at =
        |x: i32, baseline_y: i32| Rect::from_xywh(x, baseline_y.saturating_sub(h), label_w, h);

    let right_x = bbox.left.saturating_add(bbox.width());
    let left_x = bbox.left.saturating_sub(label_w);
    // Side labels sit one line below the box top so they don't overhang it.
    let side_y = bbox.top.saturating_add(h);
    let box_bottom = bbox.top.saturating_add(bbox.height());

    [
        LABEL_LAYOUT_GAP,
        LABEL_LAYOUT_GAP.saturating_add(BOX_CANDIDATE_EXT),
    ]
    .into_iter()
    .flat_map(|off| {
        [
            rect_at(bbox.left, bbox.top.saturating_sub(off).max(h)), // above
            rect_at(bbox.left, box_bottom.saturating_add(h).saturating_add(off)), // below
            rect_at(right_x.saturating_add(off), side_y),            // right
            rect_at(left_x.saturating_sub(off), side_y),             // left
        ]
    })
    .collect()
}

/// Fallback candidate positions for a deferred label, matching how the producing
/// element would have generated them: box-anchored labels ring the box,
/// point-anchored labels (keypoints) ring the point. `label` supplies the size.
pub fn candidates_for(kind: CandidateKind, anchor: Rect, label: Rect) -> Vec<Rect> {
    match kind {
        CandidateKind::Box => box_label_candidates(anchor, label.width()),
        CandidateKind::Point => {
            // The anchor is a zero-sized rect at the point.
            let (x, y) = anchor.center();
            point_label_candidates(x, y, label.width(), label.height())
        }
    }
}

/// Place all deferred labels globally and return the draw commands (label text
/// plus leader lines for displaced labels). Used by the overlay compositors.
///
/// Labels are placed highest-priority-first. Each label avoids claimed regions of
/// priority `>= its own` (reusing the coordination priority filter) and every
/// already-placed (higher- or equal-priority) label; lower-priority claims are
/// ignored, so an important label is never pushed aside by less important
/// content. A pure function, so it is unit-tested without GStreamer.
pub(crate) fn place_labels(
    width: i32,
    height: i32,
    claims: &[ClaimedRegion],
    intents: &[LabelIntent],
) -> Vec<DrawCommand> {
    // Stable sort, highest priority first; ties keep input order (deterministic).
    let mut order: Vec<usize> = (0..intents.len()).collect();
    order.sort_by(|&a, &b| intents[b].priority.cmp(&intents[a].priority));

    let mut placed: Vec<Rect> = Vec::new();
    let mut commands = Vec::new();
    for &i in &order {
        let intent = &intents[i];

        let mut registry = OccupiedRegionRegistry::new(width, height);
        // Avoid anchored content at this label's priority or higher.
        seed_registry_from_regions(&mut registry, claims, "", intent.priority);
        // Avoid labels already placed (all of priority >= this one).
        for rect in &placed {
            registry.reserve_highlight(*rect);
        }

        let candidates = candidates_for(intent.kind, intent.anchor, intent.preferred);
        let Some(placement) = place_label(&mut registry, intent.preferred, &candidates) else {
            continue; // entirely off-frame
        };
        placed.push(placement.rect);

        if placement.displaced {
            let (from, to) = leader_endpoints(intent.anchor, placement.rect);
            push_leader_line(&mut commands, from, to, intent.color);
        }
        commands.push(label_text_command(intent, placement.rect));
    }
    commands
}

/// The text draw command for a placed label, reproducing the producing element's
/// alignment: box-anchored labels are drawn bottom-left, point-anchored
/// (keypoint) labels centered.
fn label_text_command(intent: &LabelIntent, rect: Rect) -> DrawCommand {
    match intent.kind {
        CandidateKind::Box => DrawCommand::Text {
            x: rect.left as f32,
            y: rect.bottom as f32,
            text: intent.text.clone(),
            argb: intent.color,
        },
        CandidateKind::Point => {
            let (cx, cy) = rect.center();
            DrawCommand::TextCentered {
                x: cx as f32,
                y: cy as f32,
                text: intent.text.clone(),
                argb: intent.color,
            }
        }
    }
}

/// Endpoints of a leader line connecting a labelled feature to its label: the
/// point on the feature's edge nearest the label, and the point on the label's
/// edge nearest the feature. Using each rect's center to aim at the other keeps
/// the segment between the two facing edges, so it runs diagonally when the
/// label is offset both horizontally and vertically. A zero-sized `feature`
/// rect represents a point feature (e.g. a keypoint).
pub fn leader_endpoints(feature: Rect, label: Rect) -> ((i32, i32), (i32, i32)) {
    let from = feature.closest_point_to(label.center());
    let to = label.closest_point_to(feature.center());
    (from, to)
}

/// Push a leader line from a feature to a label that was displaced from its
/// default position.
pub fn push_leader_line(
    commands: &mut Vec<DrawCommand>,
    from: (i32, i32),
    to: (i32, i32),
    argb: u32,
) {
    commands.push(DrawCommand::Line {
        x0: from.0 as f32,
        y0: from.1 as f32,
        x1: to.0 as f32,
        y1: to.1 as f32,
        argb,
        width: LEADER_LINE_WIDTH,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::from_xywh(x, y, w, h)
    }

    #[test]
    fn default_position_is_used_when_free() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);

        let default = rect(10, 10, 40, 20);
        let placement = place_label(&mut registry, default, &[rect(60, 10, 40, 20)])
            .expect("expected a placement");

        assert_eq!(placement.rect, default);
        assert!(!placement.displaced);
        // Only the default region was reserved.
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn label_moves_off_a_soft_avoid_region_to_a_clear_spot() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        // A segmentation mask covers the default position.
        registry.reserve_avoid(rect(0, 0, 60, 60));

        let default = rect(10, 10, 40, 20); // sits on the mask
        let candidates = [rect(100, 100, 40, 20)]; // clear of the mask
        let placement =
            place_label(&mut registry, default, &candidates).expect("expected a placement");

        // The default is not blocked (avoid is soft), but a clear spot is
        // preferred, so the label moves and reports as displaced.
        assert_eq!(placement.rect, candidates[0]);
        assert!(placement.displaced);
    }

    #[test]
    fn label_keeps_clear_default_despite_an_avoid_region_elsewhere() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        registry.reserve_avoid(rect(100, 100, 60, 60)); // mask away from the default

        let default = rect(10, 10, 40, 20); // clear
        let placement = place_label(&mut registry, default, &[rect(60, 10, 40, 20)])
            .expect("expected a placement");

        assert_eq!(placement.rect, default);
        assert!(!placement.displaced);
    }

    #[test]
    fn label_sits_on_least_covered_mask_when_no_clear_spot_exists() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        // The left half of the frame is masked; every position overlaps it by a
        // different amount, and there are no hard regions.
        registry.reserve_avoid(rect(0, 0, 100, 200));

        let default = rect(0, 0, 40, 20); // fully masked: 40x20 = 800 px
        let candidates = [
            rect(80, 0, 40, 20), // masked 20x20 = 400 px (least)
            rect(60, 0, 40, 20), // masked 40x20 = 800 px
        ];
        let placement =
            place_label(&mut registry, default, &candidates).expect("expected a placement");

        // No hard-free, mask-free spot exists, so the label sits on the
        // least-covered mask position rather than being force-displaced.
        assert_eq!(placement.rect, candidates[0]);
        assert!(placement.displaced);
    }

    #[test]
    fn first_free_fallback_is_used_when_default_is_occupied() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);

        let default = rect(10, 10, 40, 20);
        registry.reserve_highlight(default);

        let candidates = [
            rect(10, 10, 40, 20), // collides with the highlight
            rect(60, 10, 40, 20), // free
            rect(110, 10, 40, 20),
        ];
        let placement =
            place_label(&mut registry, default, &candidates).expect("expected a placement");

        assert_eq!(placement.rect, candidates[1]);
        assert!(placement.displaced);
    }

    #[test]
    fn falls_back_to_least_overlap_when_all_candidates_collide() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        // A tall band and a thin band so candidates overlap by different areas.
        registry.reserve_highlight(rect(0, 0, 200, 30));
        registry.reserve_highlight(rect(0, 40, 200, 5));

        let default = rect(0, 0, 40, 20); // fully inside the tall band
        let candidates = [
            rect(10, 20, 40, 20), // overlaps the tall band: 40x10 = 400 px
            rect(10, 40, 40, 20), // overlaps the thin band: 40x5  = 200 px
        ];
        let placement =
            place_label(&mut registry, default, &candidates).expect("expected a forced placement");

        assert_eq!(placement.rect, candidates[1]);
        assert!(placement.displaced);
        // The forced candidate was reserved (2 highlights + 1 label).
        assert_eq!(registry.len(), 3);
    }

    #[test]
    fn least_overlap_ties_keep_the_earliest_candidate() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        registry.reserve_highlight(rect(0, 0, 200, 200));

        let default = rect(0, 0, 40, 20);
        let candidates = [rect(10, 10, 40, 20), rect(100, 100, 40, 20)];
        let placement =
            place_label(&mut registry, default, &candidates).expect("expected a forced placement");

        assert_eq!(placement.rect, candidates[0]);
        assert!(placement.displaced);
    }

    #[test]
    fn off_frame_candidates_are_skipped() {
        let mut registry = OccupiedRegionRegistry::new(100, 100);
        registry.reserve_highlight(rect(0, 0, 100, 100));

        let default = rect(0, 0, 40, 20);
        let candidates = [
            rect(200, 200, 40, 20), // entirely off-frame, must be ignored
            rect(10, 10, 40, 20),
        ];
        let placement =
            place_label(&mut registry, default, &candidates).expect("expected a forced placement");

        assert_eq!(placement.rect, candidates[1]);
        assert!(placement.displaced);
    }

    #[test]
    fn returns_none_when_nothing_intersects_the_frame() {
        let mut registry = OccupiedRegionRegistry::new(100, 100);
        registry.reserve_highlight(rect(0, 0, 100, 100));

        assert_eq!(
            place_label(
                &mut registry,
                rect(-100, -100, 40, 20),
                &[rect(200, 200, 40, 20)],
            ),
            None
        );
    }

    #[test]
    fn leader_endpoints_connect_facing_edges_diagonally() {
        // Box at (100,100)-(200,160); label sits above-left of it.
        let box_rect = Rect::from_xywh(100, 100, 100, 60);
        let label = Rect::from_xywh(20, 40, 40, 12); // (20,40)-(60,52)

        let (from, to) = leader_endpoints(box_rect, label);

        // The box endpoint is on the box, the label endpoint is on the label,
        // and the two differ in both axes (a diagonal segment).
        assert_eq!(from, box_rect.closest_point_to(label.center()));
        assert_eq!(to, label.closest_point_to(box_rect.center()));
        assert_eq!(from, (100, 100)); // box top-left corner faces the label
        assert_eq!(to, (60, 52)); // label bottom-right corner faces the box
        assert_ne!(from.0, to.0);
        assert_ne!(from.1, to.1);
    }

    #[test]
    fn leader_endpoints_handle_a_point_feature() {
        let keypoint = Rect::from_xywh(30, 30, 0, 0); // zero-sized = a point
        let label = Rect::from_xywh(50, 50, 20, 10);

        let (from, to) = leader_endpoints(keypoint, label);

        assert_eq!(from, (30, 30)); // the point itself
        assert_eq!(to, (50, 50)); // nearest label corner to the point
    }

    #[test]
    fn placement_is_deterministic_for_identical_input() {
        // The same scene built twice must yield byte-identical placement
        // decisions (no dependence on hashing / iteration order).
        let scene = || {
            let mut registry = OccupiedRegionRegistry::new(200, 200);
            registry.reserve_highlight(rect(0, 0, 200, 40));
            let default = rect(0, 0, 40, 20);
            let candidates = [
                rect(10, 20, 40, 20),
                rect(10, 45, 40, 20),
                rect(60, 45, 40, 20),
            ];
            place_label(&mut registry, default, &candidates)
        };

        assert_eq!(scene(), scene());
    }

    #[test]
    fn point_candidates_are_ordered_near_ring_then_far_ring() {
        let candidates = point_label_candidates(100, 100, 20, 10);

        // Eight per ring, two rings.
        assert_eq!(candidates.len(), 16);
        // First candidate is "above" in the near ring (gap = 4).
        assert_eq!(candidates[0], Rect::from_xywh(90, 100 - 10 - 4, 20, 10));
        // Ninth candidate is "above" in the far ring (ext = 12).
        assert_eq!(candidates[8], Rect::from_xywh(90, 100 - 10 - 12, 20, 10));
    }
}
