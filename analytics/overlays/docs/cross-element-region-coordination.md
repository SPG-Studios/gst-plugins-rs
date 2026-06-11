# Cross-element region coordination — design spike

Status: spike / prototype (in-process). Prototype lives in
`src/coordination.rs`; the object-detection and keypoints overlays are wired as
both producers and consumers.

## Problem

Overlay elements are composable: a pipeline may run several of them, plus
unrelated elements that also draw into the frame (imagine an element that adds
spikes to a person's hair). Each element today places its content using only its
own per-frame `OccupiedRegionRegistry`, with no knowledge of what other elements
have drawn. The result is that independently-authored overlays can occlude one
another — e.g. we drop a label on top of another element's rendered content.

Goal: a practical mechanism that lets composable elements **share the regions
they have claimed**, so a downstream element can avoid occluding them. The
mechanism must have a clean API that *any* element (in any plugin) can use, not
just the ones in this crate.

## Mechanism: a per-buffer "claimed regions" meta

Claimed regions are per-frame data that should travel with the frame through the
pipeline, so the natural GStreamer carrier is a **buffer meta**, not global
state. We use a custom meta registered under the well-known name
`GstAnalyticsClaimedRegions` (`coordination::CLAIMED_REGIONS_META`).

Each claim is a `ClaimedRegion { rect, kind, owner }`:

- `rect` — area in the **negotiated frame's pixel coordinate space**.
- `kind` — `Occlude` (hard: do not draw over) or `Avoid` (soft hint).
- `owner` — claiming element's name; lets a consumer skip its own claims and
  aids debugging.

The flow is read-then-claim, which makes it composable:

```
producerA  ! producerB  ! ourOverlay
   |             |              |
 claims A     reads A,        reads A+B,
              claims B        places clear of both
```

An element **reads what is already on the buffer**, places its content avoiding
those regions, then **claims the regions it used** for elements further
downstream. Because the meta is keyed by a public name, producers and consumers
need no compile-time dependency on each other — a future "hair-spikes" element in
another plugin just registers/looks up the same meta name.

### API (`coordination` module)

| Role | Function |
|------|----------|
| init | `register()` — register the meta once (called from `plugin_init`) |
| producer | `claim_commands(buffer, commands, owner)` — publish what was drawn, derived from the draw commands |
| producer | `add_claimed_regions(buffer, &[ClaimedRegion])` — append/merge claims directly |
| consumer | `claimed_regions(buffer) -> Vec<ClaimedRegion>` — read all claims |
| consumer | `seed_registry_from_claims(registry, buffer, skip_owner)` — load claims into an `OccupiedRegionRegistry`, skipping your own |

On the consume side, the overlay elements call `seed_registry_from_claims` right
after creating their registry, so the existing candidate/least-overlap placement
automatically steers around claimed areas — no change to the placement algorithm
itself. On the produce side, they call `claim_commands` after rendering.
Consuming before producing on the same buffer is what makes the chain
composable.

### Encoding

The meta stores a `gst::Structure` with two parallel arrays: `coords` (5×i32 per
region: x, y, w, h, kind) and `owners` (one string per region). Homogeneous
arrays keep it trivially readable from C, so the schema is interop-friendly.

## Why this shape

- **Per-buffer, not global.** Claims are valid for exactly one frame and must
  follow the frame across queues/threads; a buffer meta does this for free.
- **Reuses the registry.** The consumer side is a thin adapter onto the
  `OccupiedRegionRegistry` we already have — claimed regions are just
  pre-reserved highlights.
- **Name-keyed, decoupled.** Any plugin can participate; no shared Rust type
  dependency required (only the agreed name + structure schema).

## Alternatives considered

- **Reuse `GstVideoOverlayCompositionMeta`.** Elements using the overlay-
  composition path already attach rectangles with positions; a consumer could
  treat those as occupied. Useful as a *complementary* signal, but it only covers
  composition-meta overlays (not blend-mode draws), and "where I will composite"
  is not always "what must not be occluded". Worth ingesting later, but too
  narrow as the primary mechanism.
- **Shared registry via `GstContext`.** A pipeline-wide shared object negotiated
  through a context query. Better for persistent/cross-pipeline state, but
  heavyweight and awkward for per-frame data. Overkill here.
- **Out-of-band/global registry.** Breaks with threading and multiple pipelines;
  rejected.

## Productionisation path

1. **Move the meta to a shared library.** The name + `ClaimedRegion` schema
   should live in `gstreamer-analytics` (or a small shared crate) so elements in
   other plugins depend on a stable definition rather than re-deriving the
   structure layout.
2. **Scale-aware transform.** The prototype's meta transform copies regions
   verbatim across buffer copies (e.g. `videoconvert`) but does **not** rescale
   on `videoscale`. A production transform must scale `rect` using the
   meta-transform scale params. Until then, place coordinating elements in a
   single coordinate space (after any scaler).
3. **Soft (`Avoid`) handling.** Both kinds are currently reserved as hard
   highlights. `Avoid` could instead bias placement (a weighted/penalty term in
   the least-overlap step) rather than forbidding the area outright.
4. **Schema versioning.** Add a version field to the structure so the encoding
   can evolve without breaking older producers/consumers.
5. **Richer claim extents.** Producers currently claim the axis-aligned extent
   of solid content (boxes, labels, keypoint dots) and skip thin strokes
   (skeleton / leader lines). Rotated boxes claim their unrotated extent, and
   there is no property to opt out of publishing. Refine as needed.

## Status

Both `objectdetectionoverlay` and `keypointsoverlay` are now **producers and
consumers**:

- *Consume:* `analytics_to_draw_commands` seeds the registry from upstream
  claims (skipping the element's own owner) before placing.
- *Produce:* `transform_frame_ip` calls `coordination::claim_commands` after
  rendering, publishing the solid content it drew for downstream elements.

Because consuming happens before producing on the same (in-place) buffer, a
chain `ours ! videoconvert ! ours` (or ours + a third-party element) has each
stage avoid everything claimed upstream and then add its own.

## Segmentation overlay (potential soft producer)

The segmentation overlay paints per-pixel masks and places no labels, so it has
nothing to *consume*. It could, however, usefully *produce* — telling downstream
elements where its masks are. Two things must land first:

- **Real soft `Avoid`.** A mask is not "do not draw here": a label on a
  semi-transparent mask is usually fine, often desirable. Segmentation should
  publish `Avoid`, not `Occlude`. But today both kinds are reserved hard
  (productionisation item 3), and masks are large — often most of the frame — so
  publishing them now would push downstream labels into the few unmasked
  corners. Net-negative until `Avoid` is a weighted bias rather than a hard
  block.
- **Coarse extents.** Claims are rectangles; masks are arbitrary shapes. A
  segment's bounding box (e.g. "road") is a large over-approximation, so even as
  a soft hint it is blunt. Per-segment bounding boxes are the natural unit.

So segmentation is the use case that motivates finishing soft `Avoid`; wiring it
before that would hurt placement. Sequencing: (1) weighted `Avoid` in
`place_label`, then (2) segmentation publishes per-segment bounding boxes as
`Avoid`.

## Limitations (prototype)

- No rescaling across `videoscale` (see above); single shared resolution.
- `Avoid` is treated like `Occlude`.
- Only axis-aligned extents of solid content are claimed; thin strokes are not.
- Publishing is always on (no opt-out property).

## Prototype & tests

- `src/coordination.rs` — the API (`register`, `claim_commands`,
  `add_claimed_regions`, `claimed_regions`, `seed_registry_from_claims`) +
  `CustomMeta` transport.
- `render::content_bounds` — maps a draw command to the rectangle to claim.
- Wiring — both overlays consume in `analytics_to_draw_commands` and produce in
  `transform_frame_ip`.
- Tests:
  - `coordination::{round_trip_preserves_regions,
    add_claimed_regions_merges_successive_producers,
    claim_commands_publishes_solid_content_only,
    seeding_makes_the_registry_treat_claims_as_occupied,
    seeding_skips_the_consumers_own_claims}`.
  - `render::content_bounds_covers_solid_commands_and_skips_strokes`.
  - `objectdetectionoverlay::label_avoids_a_region_claimed_by_another_element`
    — end-to-end: a region claimed by a hypothetical "hair-spikes" element makes
    the OD overlay relocate its label (with a leader line) instead of occluding.
