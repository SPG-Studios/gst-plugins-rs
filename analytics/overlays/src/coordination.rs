// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Cross-element claimed-region coordination (design spike).
//!
//! Composable overlay elements need to avoid drawing on top of each other: if
//! some other element has already drawn content into part of the frame (say an
//! element that adds spikes to a person's hair), this overlay should not place
//! its labels there.
//!
//! The mechanism is a per-buffer custom meta, [`CLAIMED_REGIONS_META`], that
//! carries a list of [`ClaimedRegion`]s. Each element that draws into the frame
//! *claims* the regions it used; downstream overlay elements *read* the claims
//! and seed their [`OccupiedRegionRegistry`] so placement steers clear.
//!
//! The contract is intentionally simple and composable:
//!   * regions are expressed in the negotiated frame's pixel coordinate space;
//!   * an element claims **after** drawing and reads **what is already there**,
//!     so a chain `A ! B ! ours` lets `ours` avoid both `A` and `B`;
//!   * the meta is registered by a well-known name so *any* plugin can produce
//!     or consume it without a compile-time dependency on this crate.
//!
//! This is an in-process prototype. See `docs/cross-element-region-coordination.md`
//! for the design note, including the productionisation path (moving the meta
//! to the shared analytics library and adding a scale-aware transform).

use gst::meta::CustomMeta;

use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::render::{DrawCommand, content_bounds};

/// Well-known name of the custom buffer meta used to share claimed regions
/// between composable elements. Registered once at plugin init via [`register`].
pub const CLAIMED_REGIONS_META: &str = "GstAnalyticsClaimedRegions";

/// How strongly a claim should be honoured by downstream elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimKind {
    /// Hard claim: downstream must not draw over this region (e.g. another
    /// overlay's rendered content).
    Occlude,
    /// Soft hint: avoid drawing here if an alternative placement exists (e.g. a
    /// salient image area a detector flagged).
    Avoid,
}

impl ClaimKind {
    fn as_i32(self) -> i32 {
        match self {
            ClaimKind::Occlude => 0,
            ClaimKind::Avoid => 1,
        }
    }

    fn from_i32(value: i32) -> Self {
        match value {
            1 => ClaimKind::Avoid,
            _ => ClaimKind::Occlude,
        }
    }
}

/// A region of the frame an element has claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedRegion {
    pub rect: Rect,
    pub kind: ClaimKind,
    /// Identifier of the claiming element (factory or instance name). Lets a
    /// consumer skip its own claims and aids debugging.
    pub owner: String,
}

impl ClaimedRegion {
    /// A hard [`ClaimKind::Occlude`] claim.
    pub fn occlude(rect: Rect, owner: impl Into<String>) -> Self {
        Self {
            rect,
            kind: ClaimKind::Occlude,
            owner: owner.into(),
        }
    }
}

/// Register the claimed-regions meta. Idempotent; call once at plugin init.
pub fn register() {
    if CustomMeta::is_registered(CLAIMED_REGIONS_META) {
        return;
    }

    // Carry regions verbatim across buffer copies (e.g. a `videoconvert`
    // between two overlay elements). NOTE: scaling transforms are *not* yet
    // adjusted — a scale-aware transform is part of the productionisation work
    // described in the design note. Until then, place coordinating elements
    // after any scaler, in a single coordinate space.
    CustomMeta::register_with_transform(CLAIMED_REGIONS_META, &[], |dest, meta, _src, _type| {
        let regions = decode(meta.structure());
        if let Ok(mut dest_meta) = CustomMeta::add(dest, CLAIMED_REGIONS_META) {
            encode(dest_meta.mut_structure(), &regions);
        }
        true
    });
}

/// Publish the regions an element rendered (derived from its draw commands) as
/// claims, so downstream elements avoid occluding them. Thin strokes (skeleton
/// and leader lines) are not claimed — see [`content_bounds`].
pub fn claim_commands(buffer: &mut gst::BufferRef, commands: &[DrawCommand], owner: &str) {
    let regions: Vec<ClaimedRegion> = commands
        .iter()
        .filter_map(content_bounds)
        .map(|rect| ClaimedRegion::occlude(rect, owner))
        .collect();
    add_claimed_regions(buffer, &regions);
}

/// Append claimed regions to `buffer`, merging with any already present.
///
/// Producers call this after rendering so downstream elements can avoid the
/// areas they used.
pub fn add_claimed_regions(buffer: &mut gst::BufferRef, regions: &[ClaimedRegion]) {
    if regions.is_empty() {
        return;
    }

    let mut all = claimed_regions(buffer);
    all.extend_from_slice(regions);

    let exists = CustomMeta::from_buffer(buffer, CLAIMED_REGIONS_META).is_ok();
    let meta = if exists {
        CustomMeta::from_mut_buffer(buffer, CLAIMED_REGIONS_META)
    } else {
        CustomMeta::add(buffer, CLAIMED_REGIONS_META)
    };

    // `Err` means the meta type is not registered (see `register`); skip rather
    // than panic so a misconfigured pipeline degrades gracefully.
    if let Ok(mut meta) = meta {
        encode(meta.mut_structure(), &all);
    }
}

/// Read every claimed region currently attached to `buffer`.
pub fn claimed_regions(buffer: &gst::BufferRef) -> Vec<ClaimedRegion> {
    match CustomMeta::from_buffer(buffer, CLAIMED_REGIONS_META) {
        Ok(meta) => decode(meta.structure()),
        Err(_) => Vec::new(),
    }
}

/// Seed `registry` with every claimed region on `buffer` that was not claimed by
/// `skip_owner`, so this element's placement avoids those areas.
///
/// Both claim kinds are reserved as highlights in this prototype, which makes
/// label placement steer around them while leaving the distinction available
/// for a future weighted/soft strategy.
pub fn seed_registry_from_claims(
    registry: &mut OccupiedRegionRegistry,
    buffer: &gst::BufferRef,
    skip_owner: &str,
) {
    for region in claimed_regions(buffer) {
        if region.owner == skip_owner {
            continue;
        }
        registry.reserve_highlight(region.rect);
    }
}

// The regions are stored in the meta's `gst::Structure` as two parallel arrays:
// `coords` (5 i32 per region: x, y, w, h, kind) and `owners` (one string per
// region). Homogeneous arrays keep the encoding trivially interoperable from C.
fn encode(structure: &mut gst::StructureRef, regions: &[ClaimedRegion]) {
    let mut coords: Vec<i32> = Vec::with_capacity(regions.len() * 5);
    let mut owners: Vec<String> = Vec::with_capacity(regions.len());
    for region in regions {
        coords.extend_from_slice(&[
            region.rect.left,
            region.rect.top,
            region.rect.width(),
            region.rect.height(),
            region.kind.as_i32(),
        ]);
        owners.push(region.owner.clone());
    }
    structure.set("coords", gst::Array::new(coords));
    structure.set("owners", gst::Array::new(owners));
}

fn decode(structure: &gst::StructureRef) -> Vec<ClaimedRegion> {
    let Ok(coords) = structure.get::<gst::Array>("coords") else {
        return Vec::new();
    };
    let owners = structure.get::<gst::Array>("owners").ok();
    let coords = coords.as_slice();
    let owners = owners.as_ref().map(gst::Array::as_slice).unwrap_or(&[]);

    coords
        .chunks_exact(5)
        .enumerate()
        .map(|(index, chunk)| {
            let value = |i: usize| chunk[i].get::<i32>().unwrap_or(0);
            let owner = owners
                .get(index)
                .and_then(|v| v.get::<String>().ok())
                .unwrap_or_default();
            ClaimedRegion {
                rect: Rect::from_xywh(value(0), value(1), value(2), value(3)),
                kind: ClaimKind::from_i32(value(4)),
                owner,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::RegionPriority;
    use std::sync::Once;

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            register();
        });
    }

    #[test]
    fn round_trip_preserves_regions() {
        init();

        let input = vec![
            ClaimedRegion::occlude(Rect::from_xywh(10, 20, 30, 40), "hair-spikes"),
            ClaimedRegion {
                rect: Rect::from_xywh(100, 5, 50, 12),
                kind: ClaimKind::Avoid,
                owner: "saliency".to_string(),
            },
        ];

        let mut buffer = gst::Buffer::new();
        add_claimed_regions(buffer.make_mut(), &input);

        assert_eq!(claimed_regions(buffer.as_ref()), input);
    }

    #[test]
    fn add_claimed_regions_merges_successive_producers() {
        init();

        let mut buffer = gst::Buffer::new();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(Rect::from_xywh(0, 0, 10, 10), "a")],
        );
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(Rect::from_xywh(20, 20, 10, 10), "b")],
        );

        let regions = claimed_regions(buffer.as_ref());
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].owner, "a");
        assert_eq!(regions[1].owner, "b");
    }

    #[test]
    fn seeding_makes_the_registry_treat_claims_as_occupied() {
        init();

        let claimed = Rect::from_xywh(40, 40, 60, 30);
        let mut buffer = gst::Buffer::new();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(claimed, "hair-spikes")],
        );

        let mut registry = OccupiedRegionRegistry::new(200, 200);
        seed_registry_from_claims(&mut registry, buffer.as_ref(), "odoverlay");

        // The claimed area is now occupied, so a label cannot be reserved there.
        assert!(registry.is_occupied(RegionPriority::Label, claimed));
        assert!(!registry.reserve_label(Rect::from_xywh(50, 50, 20, 10)));
        // Elsewhere is still free.
        assert!(registry.reserve_label(Rect::from_xywh(150, 150, 20, 10)));
    }

    #[test]
    fn claim_commands_publishes_solid_content_only() {
        init();

        let commands = vec![
            DrawCommand::Rectangle {
                x: 10.0,
                y: 20.0,
                width: 40.0,
                height: 30.0,
                rotation: 0.0,
                argb: 0,
                filled: false,
            },
            DrawCommand::Text {
                x: 10.0,
                y: 20.0,
                text: "person".to_string(),
                argb: 0,
            },
            // A leader line — must NOT be claimed.
            DrawCommand::Line {
                x0: 0.0,
                y0: 0.0,
                x1: 9.0,
                y1: 9.0,
                argb: 0,
                width: 1.0,
            },
        ];

        let mut buffer = gst::Buffer::new();
        claim_commands(buffer.make_mut(), &commands, "odoverlay");

        let regions = claimed_regions(buffer.as_ref());
        // Rectangle + Text are claimed; the Line is not.
        assert_eq!(regions.len(), 2);
        assert!(regions.iter().all(|r| r.owner == "odoverlay"));
        assert!(
            regions
                .iter()
                .any(|r| r.rect == Rect::from_xywh(10, 20, 40, 30))
        );
    }

    #[test]
    fn seeding_skips_the_consumers_own_claims() {
        init();

        let mut buffer = gst::Buffer::new();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(
                Rect::from_xywh(10, 10, 40, 40),
                "odoverlay",
            )],
        );

        let mut registry = OccupiedRegionRegistry::new(200, 200);
        seed_registry_from_claims(&mut registry, buffer.as_ref(), "odoverlay");

        // Our own prior claim must not block our placement.
        assert!(registry.is_empty());
    }
}
