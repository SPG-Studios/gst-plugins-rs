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
//! The meta carries a coordinate-aware transform (see [`crate::meta_transform`]),
//! modelled on `GstVideoRegionOfInterestMeta`: a scale/crop/letterbox transform
//! between coordinating elements (`ours ! videoscale ! ours`) maps the claims
//! into the downstream coordinate space automatically; plain copies carry them
//! verbatim. See `docs/cross-element-region-coordination.md` for the design note
//! and the remaining productionisation path (moving the meta to the shared
//! analytics library).

use gst::glib;
use gst::meta::CustomMeta;
use gst::prelude::*;

use crate::geometry::{OccupiedRegionRegistry, Rect};
use crate::meta_transform::register_rect_transform;
use crate::render::{DrawCommand, content_bounds};

/// Well-known name of the custom buffer meta used to share claimed regions
/// between composable elements. Registered once at plugin init via [`register`].
pub const CLAIMED_REGIONS_META: &str = "GstAnalyticsClaimedRegions";

/// Default element priority. All overlays default to the same value, so out of
/// the box every element respects every other (equal priorities mutually avoid).
/// An application or auto-plugging bin raises priority on the elements whose
/// content should win conflicts; downstream lower-priority content is overdrawn.
pub(crate) const DEFAULT_PRIORITY: i32 = 0;

/// The shared `priority` GObject property, installed by every overlay element so
/// they expose one consistent priority knob.
pub(crate) fn priority_param_spec() -> glib::ParamSpec {
    glib::ParamSpecInt::builder("priority")
        .nick("Priority")
        .blurb(
            "Cross-element priority: an overlay respects claimed regions of priority >= its own \
             and overdraws lower-priority ones. Higher wins; equal priorities mutually avoid.",
        )
        .default_value(DEFAULT_PRIORITY)
        .mutable_playing()
        .build()
}

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
///
/// Three orthogonal properties describe a claim:
///   * `kind` — how others must treat the space it occupies (hard [`ClaimKind::Occlude`]
///     = don't draw over it; soft [`ClaimKind::Avoid`] = prefer not to). This is
///     about how *others* treat *this* content.
///   * `priority` — how important the content is. A consumer placing content of
///     priority `P` honours claims with priority `>= P` and overdraws claims with
///     priority `< P`. Higher wins.
///
/// Note `kind` (hard/soft) is *not* the same as movability (whether the producing
/// content can itself relocate) — e.g. a keypoint and a label are both `Occlude`
/// yet a keypoint is anchored and a label is free. Movability lives on the
/// producer's placement path, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedRegion {
    pub rect: Rect,
    pub kind: ClaimKind,
    /// Identifier of the claiming element (factory or instance name). Lets a
    /// consumer skip its own claims and aids debugging.
    pub owner: String,
    /// Importance of the claimed content; consumers overdraw claims of lower
    /// priority than the content they are placing. Higher = more important.
    pub priority: i32,
}

impl ClaimedRegion {
    /// A hard [`ClaimKind::Occlude`] claim at the given priority.
    pub fn occlude(rect: Rect, owner: impl Into<String>, priority: i32) -> Self {
        Self {
            rect,
            kind: ClaimKind::Occlude,
            owner: owner.into(),
            priority,
        }
    }

    /// A soft [`ClaimKind::Avoid`] claim at the given priority.
    pub fn avoid(rect: Rect, owner: impl Into<String>, priority: i32) -> Self {
        Self {
            rect,
            kind: ClaimKind::Avoid,
            owner: owner.into(),
            priority,
        }
    }
}

/// Register the claimed-regions meta. Idempotent; call once at plugin init.
pub fn register() {
    // Tagged `video`+`size` so a scaler/cropper (`videoconvertscale`,
    // `glcolorscale`, …) invokes our transform to map the claims into its output
    // coordinate space; on a plain copy (e.g. a same-size `videoconvert`) they
    // are carried verbatim. A region cropped entirely out of frame is dropped.
    // This lets coordinating elements sit on either side of such a transform.
    register_rect_transform(
        CLAIMED_REGIONS_META,
        &["video", "size"],
        |src_meta, dest, map| {
            let regions: Vec<ClaimedRegion> = decode(src_meta.structure())
                .into_iter()
                .filter_map(|mut region| {
                    region.rect = map(region.rect)?;
                    Some(region)
                })
                .collect();
            if let Ok(mut dest_meta) = CustomMeta::add(dest, CLAIMED_REGIONS_META) {
                encode(dest_meta.mut_structure(), &regions);
            }
            true
        },
    );
}

/// Publish the regions an element rendered (derived from its draw commands) as
/// claims, so downstream elements avoid occluding them. Thin strokes (skeleton
/// and leader lines) are not claimed — see [`content_bounds`]. The claim kind is
/// derived per command by [`command_claim_kind`]; all claims carry the element's
/// `priority`.
pub fn claim_commands(
    buffer: &mut gst::BufferRef,
    commands: &[DrawCommand],
    owner: &str,
    priority: i32,
) {
    let regions: Vec<ClaimedRegion> = commands
        .iter()
        .filter_map(|command| {
            let rect = content_bounds(command)?;
            Some(match command_claim_kind(command) {
                ClaimKind::Occlude => ClaimedRegion::occlude(rect, owner, priority),
                ClaimKind::Avoid => ClaimedRegion::avoid(rect, owner, priority),
            })
        })
        .collect();
    add_claimed_regions(buffer, &regions);
}

/// The claim kind a drawn command warrants. An outline-only box is mostly
/// transparent inside, so it is a soft [`ClaimKind::Avoid`]; everything else
/// solid (a filled box, a text label, a keypoint marker) is a hard
/// [`ClaimKind::Occlude`] that downstream content must not draw over.
fn command_claim_kind(command: &DrawCommand) -> ClaimKind {
    match command {
        DrawCommand::Rectangle { filled: false, .. } => ClaimKind::Avoid,
        _ => ClaimKind::Occlude,
    }
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

/// Seed `registry` with the claimed regions on `buffer` that this element must
/// respect, so its placement steers around those areas.
///
/// A region is respected when it was not claimed by `skip_owner` **and** its
/// priority is `>= self_priority` — i.e. it belongs to content at least as
/// important as what this element is placing. Lower-priority claims are dropped
/// (not seeded), so the element places freely over them and, being downstream,
/// overdraws them ("higher priority wins").
///
/// For respected claims the kind maps to a placement priority:
/// [`ClaimKind::Occlude`] becomes a hard highlight (labels must not overlap it),
/// while [`ClaimKind::Avoid`] becomes a soft region (labels prefer not to overlap
/// it — e.g. a segmentation mask — but may when no clear alternative exists).
pub fn seed_registry_from_claims(
    registry: &mut OccupiedRegionRegistry,
    buffer: &gst::BufferRef,
    skip_owner: &str,
    self_priority: i32,
) {
    seed_registry_from_regions(
        registry,
        &claimed_regions(buffer),
        skip_owner,
        self_priority,
    );
}

/// Like [`seed_registry_from_claims`] but seeds from an already-read slice of
/// regions. Used by the compositor, which reads the claims once and then seeds a
/// fresh registry per label (at that label's priority).
pub fn seed_registry_from_regions(
    registry: &mut OccupiedRegionRegistry,
    regions: &[ClaimedRegion],
    skip_owner: &str,
    self_priority: i32,
) {
    for region in regions {
        if region.owner == skip_owner || region.priority < self_priority {
            continue;
        }
        match region.kind {
            ClaimKind::Occlude => registry.reserve_highlight(region.rect),
            ClaimKind::Avoid => registry.reserve_avoid(region.rect),
        };
    }
}

/// Number of `i32` values packed per region in the meta's `coords` array, in
/// order: x, y, w, h, kind, priority.
const COORDS_PER_REGION: usize = 6;

// The regions are stored in the meta's `gst::Structure` as two parallel arrays:
// `coords` ([`COORDS_PER_REGION`] i32 per region: x, y, w, h, kind, priority)
// and `owners` (one string per region). Homogeneous arrays keep the encoding
// trivially interoperable from C.
fn encode(structure: &mut gst::StructureRef, regions: &[ClaimedRegion]) {
    let mut coords: Vec<i32> = Vec::with_capacity(regions.len() * COORDS_PER_REGION);
    let mut owners: Vec<String> = Vec::with_capacity(regions.len());
    for region in regions {
        coords.extend_from_slice(&[
            region.rect.left,
            region.rect.top,
            region.rect.width(),
            region.rect.height(),
            region.kind.as_i32(),
            region.priority,
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
        .chunks_exact(COORDS_PER_REGION)
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
                priority: value(5),
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
            ClaimedRegion::occlude(Rect::from_xywh(10, 20, 30, 40), "hair-spikes", 5),
            ClaimedRegion {
                rect: Rect::from_xywh(100, 5, 50, 12),
                kind: ClaimKind::Avoid,
                owner: "saliency".to_string(),
                priority: 7,
            },
        ];

        let mut buffer = gst::Buffer::new();
        add_claimed_regions(buffer.make_mut(), &input);

        // Round-trips rect, kind, owner and priority.
        assert_eq!(claimed_regions(buffer.as_ref()), input);
    }

    // A `videoscale` between two coordinating elements rescales the claims into
    // its output coordinate space, so downstream reads them in its own space.
    #[test]
    fn claims_rescale_across_videoscale() {
        init();

        let mut h = gst_check::Harness::new("videoscale");
        h.set_caps_str(
            "video/x-raw,format=RGBA,width=100,height=100,framerate=30/1",
            "video/x-raw,format=RGBA,width=200,height=200,framerate=30/1",
        );

        let mut buffer = gst::Buffer::with_size(100 * 100 * 4).unwrap();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(
                Rect::from_xywh(10, 20, 30, 40),
                "od",
                4,
            )],
        );

        let out = h
            .push_and_pull(buffer)
            .expect("videoscale should output a scaled buffer");

        let regions = claimed_regions(out.as_ref());
        assert_eq!(regions.len(), 1);
        // 100x100 -> 200x200 doubles every coordinate; kind/owner/priority survive.
        assert_eq!(regions[0].rect, Rect::from_xywh(20, 40, 60, 80));
        assert_eq!(regions[0].kind, ClaimKind::Occlude);
        assert_eq!(regions[0].owner, "od");
        assert_eq!(regions[0].priority, 4);
    }

    // With letterbox borders (aspect change + add-borders), the matrix transform
    // offsets the claims by the border, not just a stretch — like the ROI meta.
    #[test]
    fn claims_offset_by_letterbox_borders() {
        init();

        let mut h = gst_check::Harness::new("videoscale");
        // Keep DAR and pad: a 100x100 (1:1) frame fitted into 300x100 stays
        // 100 wide, centred, leaving a 100px pillarbox each side.
        // Pin PAR=1/1 on both sides so videoscale must letterbox (it can't keep
        // DAR by choosing an output pixel-aspect-ratio).
        h.element().unwrap().set_property("add-borders", true);
        h.set_caps_str(
            "video/x-raw,format=RGBA,width=100,height=100,framerate=30/1,pixel-aspect-ratio=1/1",
            "video/x-raw,format=RGBA,width=300,height=100,framerate=30/1,pixel-aspect-ratio=1/1",
        );

        let mut buffer = gst::Buffer::with_size(100 * 100 * 4).unwrap();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(
                Rect::from_xywh(10, 20, 30, 40),
                "od",
                0,
            )],
        );

        let out = h
            .push_and_pull(buffer)
            .expect("videoscale should output a letterboxed buffer");

        let regions = claimed_regions(out.as_ref());
        assert_eq!(regions.len(), 1);
        // No vertical scale (100->100), shifted right by the 100px border; a plain
        // stretch would instead give x=30, w=90.
        assert_eq!(regions[0].rect, Rect::from_xywh(110, 20, 30, 40));
    }

    // A plain copy (no scaling) carries the claims verbatim, as before.
    #[test]
    fn claims_copy_verbatim_on_deep_copy() {
        init();

        let input = vec![ClaimedRegion::occlude(
            Rect::from_xywh(10, 20, 30, 40),
            "od",
            4,
        )];
        let mut buffer = gst::Buffer::new();
        add_claimed_regions(buffer.make_mut(), &input);

        // A deep copy invokes the meta transform with the copy quark → verbatim.
        let copied = buffer.copy_deep().unwrap();
        assert_eq!(claimed_regions(copied.as_ref()), input);
    }

    #[test]
    fn add_claimed_regions_merges_successive_producers() {
        init();

        let mut buffer = gst::Buffer::new();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(
                Rect::from_xywh(0, 0, 10, 10),
                "a",
                0,
            )],
        );
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::occlude(
                Rect::from_xywh(20, 20, 10, 10),
                "b",
                0,
            )],
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
            &[ClaimedRegion::occlude(claimed, "hair-spikes", 0)],
        );

        let mut registry = OccupiedRegionRegistry::new(200, 200);
        seed_registry_from_claims(&mut registry, buffer.as_ref(), "odoverlay", 0);

        // The claimed area is now occupied, so a label cannot be reserved there.
        assert!(registry.is_occupied(RegionPriority::Label, claimed));
        assert!(!registry.reserve_label(Rect::from_xywh(50, 50, 20, 10)));
        // Elsewhere is still free.
        assert!(registry.reserve_label(Rect::from_xywh(150, 150, 20, 10)));
    }

    #[test]
    fn avoid_claims_are_seeded_as_soft_regions() {
        init();

        let claimed = Rect::from_xywh(40, 40, 60, 30);
        let mut buffer = gst::Buffer::new();
        add_claimed_regions(
            buffer.make_mut(),
            &[ClaimedRegion::avoid(claimed, "segoverlay", 0)],
        );

        let mut registry = OccupiedRegionRegistry::new(200, 200);
        seed_registry_from_claims(&mut registry, buffer.as_ref(), "odoverlay", 0);

        // Unlike an Occlude claim, an Avoid claim is soft: it does not block a
        // label (so the label may still be placed there when forced) ...
        assert!(!registry.is_occupied(RegionPriority::Label, claimed));
        assert!(registry.reserve_label(Rect::from_xywh(50, 50, 20, 10)));
        // ... but it is measurable, so placement steers around it when it can.
        assert!(registry.avoid_overlap_area(claimed).unwrap() > 0);
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
        claim_commands(buffer.make_mut(), &commands, "odoverlay", 0);

        let regions = claimed_regions(buffer.as_ref());
        // Rectangle + Text are claimed; the Line is not.
        assert_eq!(regions.len(), 2);
        assert!(regions.iter().all(|r| r.owner == "odoverlay"));

        // The outline box (filled: false) is a soft Avoid; the text label is a
        // hard Occlude.
        let box_region = regions
            .iter()
            .find(|r| r.rect == Rect::from_xywh(10, 20, 40, 30))
            .expect("box region claimed");
        assert_eq!(box_region.kind, ClaimKind::Avoid);
        let text_region = regions
            .iter()
            .find(|r| r.rect != Rect::from_xywh(10, 20, 40, 30))
            .expect("text region claimed");
        assert_eq!(text_region.kind, ClaimKind::Occlude);
    }

    #[test]
    fn claim_commands_marks_filled_boxes_as_occlude() {
        init();

        let commands = vec![DrawCommand::Rectangle {
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 20.0,
            rotation: 0.0,
            argb: 0,
            filled: true,
        }];

        let mut buffer = gst::Buffer::new();
        claim_commands(buffer.make_mut(), &commands, "odoverlay", 3);

        let regions = claimed_regions(buffer.as_ref());
        assert_eq!(regions.len(), 1);
        // A filled box is opaque, so it is a hard Occlude.
        assert_eq!(regions[0].kind, ClaimKind::Occlude);
        // All claims carry the element priority passed to claim_commands.
        assert_eq!(regions[0].priority, 3);
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
                0,
            )],
        );

        let mut registry = OccupiedRegionRegistry::new(200, 200);
        seed_registry_from_claims(&mut registry, buffer.as_ref(), "odoverlay", 0);

        // Our own prior claim must not block our placement.
        assert!(registry.is_empty());
    }

    #[test]
    fn seeding_ignores_lower_priority_claims_but_honours_higher() {
        init();

        let low = Rect::from_xywh(10, 10, 40, 40);
        let high = Rect::from_xywh(120, 120, 40, 40);
        let mut buffer = gst::Buffer::new();
        add_claimed_regions(
            buffer.make_mut(),
            &[
                ClaimedRegion::occlude(low, "label-overlay", 1),
                ClaimedRegion::occlude(high, "keypoint-overlay", 9),
            ],
        );

        // We place content at priority 5: we must respect the priority-9 claim
        // (avoid it) but overdraw the priority-1 claim (it is not seeded).
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        seed_registry_from_claims(&mut registry, buffer.as_ref(), "ours", 5);

        assert!(
            !registry.is_occupied(RegionPriority::Label, low),
            "lower-priority claim should be overdrawn, not avoided"
        );
        assert!(
            registry.is_occupied(RegionPriority::Label, high),
            "higher-priority claim must still be avoided"
        );
    }
}
