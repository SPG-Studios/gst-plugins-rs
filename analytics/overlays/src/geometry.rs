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

    #[allow(dead_code)]
    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    #[allow(dead_code)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegionPriority {
    Highlight = 0,
    Label = 1,
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

    pub fn reserve_highlight(&mut self, rect: Rect) -> bool {
        self.reserve(RegionPriority::Highlight, rect)
    }

    pub fn reserve_label(&mut self, rect: Rect) -> bool {
        self.reserve(RegionPriority::Label, rect)
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
}
