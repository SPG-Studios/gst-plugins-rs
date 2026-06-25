// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn from_xywh(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            left: x,
            top: y,
            right: x.saturating_add(width),
            bottom: y.saturating_add(height),
        }
    }

    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    pub fn is_empty(self) -> bool {
        self.left >= self.right || self.top >= self.bottom
    }

    pub fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let rect = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };

        (!rect.is_empty()).then_some(rect)
    }

    /// Center point of the rectangle, used as a leader-line endpoint.
    pub fn center(self) -> (i32, i32) {
        (
            self.left + (self.right - self.left) / 2,
            self.top + (self.bottom - self.top) / 2,
        )
    }

    /// Point on this rectangle (clamped to its bounds) nearest to `point`. For a
    /// point outside the rectangle this lands on the nearest edge, which makes a
    /// tidier leader-line endpoint than the center.
    pub fn closest_point_to(self, (x, y): (i32, i32)) -> (i32, i32) {
        (
            x.max(self.left).min(self.right),
            y.max(self.top).min(self.bottom),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegionPriority {
    /// Hard, model-fixed content (bounding boxes, keypoint markers) and hard
    /// cross-element claims. Labels must not overlap these.
    Highlight = 0,
    /// A placed label. Other labels avoid it.
    Label = 1,
    /// A soft region (a segmentation mask, an outline-box claim): labels prefer
    /// not to overlap it, but may when no hard-free, mask-free spot exists. It
    /// never *blocks* placement — only [`OccupiedRegionRegistry::avoid_overlap_area`]
    /// reads it, so it acts as a tie-breaker rather than an obstacle.
    Avoid = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OccupiedRegion {
    priority: RegionPriority,
    rect: Rect,
}

#[derive(Debug, Clone)]
pub struct OccupiedRegionRegistry {
    frame: Rect,
    regions: Vec<OccupiedRegion>,
}

impl OccupiedRegionRegistry {
    pub fn new(frame_width: i32, frame_height: i32) -> Self {
        Self {
            frame: Rect::from_xywh(0, 0, frame_width, frame_height),
            regions: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self, frame_width: i32, frame_height: i32) {
        self.frame = Rect::from_xywh(0, 0, frame_width, frame_height);
        self.regions.clear();
    }

    /// Register a highlight region (a bounding box or keypoint marker).
    ///
    /// Highlights are fixed at the positions the model dictates, so they always
    /// register and may overlap one another. Labels, reserved at a lower
    /// priority, still avoid them. Returns `false` only when the rect lies
    /// entirely outside the frame.
    pub fn reserve_highlight(&mut self, rect: Rect) -> bool {
        self.force_reserve(RegionPriority::Highlight, rect)
            .is_some()
    }

    pub fn reserve_label(&mut self, rect: Rect) -> bool {
        self.reserve(RegionPriority::Label, rect)
    }

    /// Register a soft [`RegionPriority::Avoid`] region (a segmentation mask or
    /// an outline-box claim). Like a highlight it is always recorded and may
    /// overlap anything, but unlike a highlight it does not *block* label
    /// placement — labels only steer around it when a clear alternative exists
    /// (see [`Self::avoid_overlap_area`] and `placement::place_label`).
    pub fn reserve_avoid(&mut self, rect: Rect) -> bool {
        self.force_reserve(RegionPriority::Avoid, rect).is_some()
    }

    pub fn reserve(&mut self, priority: RegionPriority, rect: Rect) -> bool {
        let Some(clipped) = rect.intersection(self.frame) else {
            return false;
        };

        if self
            .regions
            .iter()
            .any(|region| region.priority <= priority && region.rect.intersects(clipped))
        {
            return false;
        }

        self.regions.push(OccupiedRegion {
            priority,
            rect: clipped,
        });

        true
    }

    #[allow(dead_code)]
    pub fn is_occupied(&self, priority: RegionPriority, rect: Rect) -> bool {
        let Some(clipped) = rect.intersection(self.frame) else {
            return false;
        };

        self.regions
            .iter()
            .any(|region| region.priority <= priority && region.rect.intersects(clipped))
    }

    /// Total area where `rect`, clipped to the frame, overlaps already-reserved
    /// regions of priority `priority` or higher. Returns `None` when `rect`
    /// lies entirely outside the frame.
    pub fn overlap_area(&self, priority: RegionPriority, rect: Rect) -> Option<i64> {
        let clipped = rect.intersection(self.frame)?;

        let area = self
            .regions
            .iter()
            .filter(|region| region.priority <= priority)
            .filter_map(|region| region.rect.intersection(clipped))
            .map(|overlap| i64::from(overlap.width()) * i64::from(overlap.height()))
            .sum();

        Some(area)
    }

    pub fn label_overlap_area(&self, rect: Rect) -> Option<i64> {
        self.overlap_area(RegionPriority::Label, rect)
    }

    /// Total area where `rect`, clipped to the frame, overlaps soft
    /// [`RegionPriority::Avoid`] regions only (segmentation masks, outline-box
    /// claims). Used to prefer a clear label position over one that sits on a
    /// mask. Returns `None` when `rect` lies entirely outside the frame.
    pub fn avoid_overlap_area(&self, rect: Rect) -> Option<i64> {
        let clipped = rect.intersection(self.frame)?;

        let area = self
            .regions
            .iter()
            .filter(|region| region.priority == RegionPriority::Avoid)
            .filter_map(|region| region.rect.intersection(clipped))
            .map(|overlap| i64::from(overlap.width()) * i64::from(overlap.height()))
            .sum();

        Some(area)
    }

    /// Reserve `rect`, clipped to the frame, unconditionally — even when it
    /// overlaps existing regions. Returns the clipped rect that was inserted,
    /// or `None` if `rect` lies entirely outside the frame.
    pub fn force_reserve(&mut self, priority: RegionPriority, rect: Rect) -> Option<Rect> {
        let clipped = rect.intersection(self.frame)?;
        self.regions.push(OccupiedRegion {
            priority,
            rect: clipped,
        });
        Some(clipped)
    }

    pub fn force_reserve_label(&mut self, rect: Rect) -> Option<Rect> {
        self.force_reserve(RegionPriority::Label, rect)
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_intersection_and_dimensions_work() {
        let rect = Rect::from_xywh(10, 20, 30, 40);

        assert_eq!(rect.left, 10);
        assert_eq!(rect.top, 20);
        assert_eq!(rect.right, 40);
        assert_eq!(rect.bottom, 60);
        assert_eq!(rect.width(), 30);
        assert_eq!(rect.height(), 40);
        assert!(!rect.is_empty());
    }

    #[test]
    fn registry_clips_regions_to_frame_bounds() {
        let mut registry = OccupiedRegionRegistry::new(100, 100);

        assert!(registry.reserve_highlight(Rect::from_xywh(-10, -10, 30, 30)));
        assert!(registry.is_occupied(RegionPriority::Label, Rect::from_xywh(0, 0, 1, 1)));
        assert!(!registry.is_occupied(RegionPriority::Label, Rect::from_xywh(40, 40, 1, 1)));
    }

    #[test]
    fn labels_do_not_overlap_existing_highlights() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);

        assert!(registry.reserve_highlight(Rect::from_xywh(10, 10, 40, 20)));
        assert!(!registry.reserve_label(Rect::from_xywh(20, 15, 30, 10)));
        assert!(registry.reserve_label(Rect::from_xywh(80, 80, 20, 10)));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn highlights_may_overlap_each_other() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);

        // Boxes / keypoint markers are fixed by the model, so overlapping ones
        // all register (unlike labels, which cannot overlap).
        assert!(registry.reserve_highlight(Rect::from_xywh(10, 10, 40, 40)));
        assert!(registry.reserve_highlight(Rect::from_xywh(30, 30, 40, 40)));
        assert_eq!(registry.len(), 2);

        // A label may not overlap either highlight, but is free elsewhere.
        assert!(!registry.reserve_label(Rect::from_xywh(35, 35, 10, 10)));
        assert!(registry.reserve_label(Rect::from_xywh(120, 120, 20, 10)));
    }

    #[test]
    fn higher_priority_regions_can_be_added_after_labels() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);

        assert!(registry.reserve_label(Rect::from_xywh(20, 20, 20, 20)));
        assert!(registry.reserve_highlight(Rect::from_xywh(25, 25, 10, 10)));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn clearing_a_frame_resets_the_registry() {
        let mut registry = OccupiedRegionRegistry::new(100, 100);

        assert!(registry.reserve_highlight(Rect::from_xywh(0, 0, 20, 20)));
        registry.clear(50, 50);

        assert!(registry.is_empty());
        assert!(registry.reserve_label(Rect::from_xywh(0, 0, 20, 20)));
    }

    #[test]
    fn rect_center_is_the_midpoint() {
        assert_eq!(Rect::from_xywh(10, 20, 40, 20).center(), (30, 30));
    }

    #[test]
    fn closest_point_to_lands_on_the_nearest_edge() {
        let rect = Rect::from_xywh(10, 20, 40, 20); // left 10, top 20, right 50, bottom 40

        // Feature above-left clamps to the top-left corner.
        assert_eq!(rect.closest_point_to((0, 0)), (10, 20));
        // Feature directly below clamps onto the bottom edge at the same x.
        assert_eq!(rect.closest_point_to((30, 100)), (30, 40));
        // Feature to the right clamps onto the right edge at the same y.
        assert_eq!(rect.closest_point_to((90, 25)), (50, 25));
        // Feature inside the rect is returned unchanged.
        assert_eq!(rect.closest_point_to((30, 30)), (30, 30));
    }

    #[test]
    fn overlap_area_sums_intersections_with_relevant_regions() {
        let mut registry = OccupiedRegionRegistry::new(100, 100);
        registry.reserve_highlight(Rect::from_xywh(0, 0, 20, 20));
        registry.reserve_highlight(Rect::from_xywh(30, 0, 20, 20));

        // A label rect overlapping the first highlight by 10x20 and the second
        // by 10x20 => 200 + 200 = 400.
        let area = registry
            .label_overlap_area(Rect::from_xywh(10, 0, 30, 20))
            .expect("rect intersects the frame");
        assert_eq!(area, 400);

        // No overlap with any region.
        assert_eq!(
            registry.label_overlap_area(Rect::from_xywh(60, 60, 10, 10)),
            Some(0)
        );

        // Entirely outside the frame.
        assert_eq!(
            registry.label_overlap_area(Rect::from_xywh(200, 200, 10, 10)),
            None
        );
    }

    #[test]
    fn force_reserve_inserts_even_when_overlapping() {
        let mut registry = OccupiedRegionRegistry::new(100, 100);
        registry.reserve_highlight(Rect::from_xywh(0, 0, 50, 50));

        // A normal reserve would refuse this overlapping label.
        assert!(!registry.reserve_label(Rect::from_xywh(10, 10, 20, 20)));
        // force_reserve_label inserts it anyway, clipped to the frame.
        assert_eq!(
            registry.force_reserve_label(Rect::from_xywh(90, 90, 40, 40)),
            Some(Rect::from_xywh(90, 90, 10, 10))
        );
        // Off-frame rects are still rejected.
        assert_eq!(
            registry.force_reserve_label(Rect::from_xywh(200, 200, 10, 10)),
            None
        );
    }

    #[test]
    fn avoid_regions_are_soft_and_measured() {
        let mut registry = OccupiedRegionRegistry::new(200, 200);
        registry.reserve_avoid(Rect::from_xywh(0, 0, 100, 100));

        // Unlike a highlight, an avoid region does not block a label (it is soft).
        assert!(!registry.is_occupied(RegionPriority::Label, Rect::from_xywh(10, 10, 20, 20)));
        assert!(registry.reserve_label(Rect::from_xywh(10, 10, 20, 20)));

        // The overlap with avoid regions is measurable so placement can rank it.
        assert_eq!(
            registry.avoid_overlap_area(Rect::from_xywh(90, 90, 20, 20)),
            Some(10 * 10)
        );

        // avoid_overlap_area counts only avoid regions, not hard ones.
        registry.reserve_highlight(Rect::from_xywh(150, 150, 20, 20));
        assert_eq!(
            registry.avoid_overlap_area(Rect::from_xywh(150, 150, 20, 20)),
            Some(0)
        );
    }
}
