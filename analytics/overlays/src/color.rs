// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/// RGB color representation (0-255 range for each component)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    /// Create RGB color from components
    #[allow(dead_code)]
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        RgbColor { r, g, b }
    }

    /// Convert to ARGB u32 with full opacity
    pub fn to_argb(self) -> u32 {
        (0xFF << 24) | ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }
}

/// HSV color representation (H, S, V all in 0.0-1.0 range)
#[derive(Debug, Clone, Copy)]
pub struct HsvColor {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl HsvColor {
    /// Create HSV color from components
    #[allow(dead_code)]
    pub fn new(h: f32, s: f32, v: f32) -> Self {
        HsvColor { h, s, v }
    }

    /// Convert HSV to RGB color space
    ///
    /// Uses the standard HSV to RGB conversion algorithm.
    /// Input values should be in range [0.0, 1.0].
    pub fn to_rgb(self) -> RgbColor {
        let h = self.h;
        let s = self.s;
        let v = self.v;

        let hi = (h * 6.0) as i32;
        let f = h * 6.0 - hi as f32;
        let p = v * (1.0 - s);
        let q = v * (1.0 - f * s);
        let t = v * (1.0 - (1.0 - f) * s);

        let (r, g, b) = match hi.rem_euclid(6) {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            5 => (v, p, q),
            _ => (0.0, 0.0, 0.0),
        };

        RgbColor {
            r: (r * 255.0) as u8,
            g: (g * 255.0) as u8,
            b: (b * 255.0) as u8,
        }
    }

    /// Convert HSV to ARGB u32 with full opacity
    pub fn to_argb(self) -> u32 {
        self.to_rgb().to_argb()
    }
}

/// Generate a perceptually distributed color based on a track ID.
///
/// Uses a binary distribution algorithm to spread hue across the HSV color wheel.
/// This ensures visually distinct colors for consecutive track IDs, making it easy
/// to distinguish between different tracked objects.
///
/// # Arguments
/// * `track_id` - Unique identifier for the track
/// * `saturation` - Saturation component (0.0-1.0), default suggested: 0.85
/// * `value` - Value component (0.0-1.0), default suggested: 0.95
///
/// # Returns
/// RGB color with values in 0-255 range
#[allow(dead_code)]
pub fn generate_track_color_rgb(track_id: u64, saturation: f32, value: f32) -> RgbColor {
    generate_track_color_hsv(track_id, saturation, value).to_rgb()
}

/// Generate a perceptually distributed color based on a track ID (HSV version).
///
/// Uses a binary distribution algorithm to spread hue across the HSV color wheel.
/// This ensures visually distinct colors for consecutive track IDs.
///
/// # Arguments
/// * `track_id` - Unique identifier for the track
/// * `saturation` - Saturation component (0.0-1.0), typical: 0.85
/// * `value` - Value component (0.0-1.0), typical: 0.95
///
/// # Returns
/// HSV color with components in 0.0-1.0 range
pub fn generate_track_color_hsv(track_id: u64, saturation: f32, value: f32) -> HsvColor {
    // Van der Corput base-2 sequence: bit-reverse the track id into the
    // fractional hue range [0, 1). Reversing *all* bits (rather than stopping at
    // the most-significant set bit) is what keeps consecutive ids distinct and
    // well spread: 0 -> 0.0, 1 -> 0.5, 2 -> 0.25, 3 -> 0.75, ...
    let mut h = 0.0_f32;
    let mut increment = 0.5_f32;

    let mut id = track_id;
    while id > 0 {
        if id & 1 == 1 {
            h += increment;
        }
        id >>= 1;
        increment *= 0.5;
    }

    HsvColor {
        h,
        s: saturation,
        v: value,
    }
}

/// Generate a perceptually distributed ARGB color based on a track ID.
///
/// Convenience function that combines track ID color generation with ARGB conversion.
///
/// # Arguments
/// * `track_id` - Unique identifier for the track
/// * `saturation` - Saturation component (0.0-1.0), typical: 0.85
/// * `value` - Value component (0.0-1.0), typical: 0.95
///
/// # Returns
/// ARGB u32 color with full opacity (alpha=0xFF)
pub fn generate_track_color_argb(track_id: u64, saturation: f32, value: f32) -> u32 {
    generate_track_color_hsv(track_id, saturation, value).to_argb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_to_rgb_red() {
        let hsv = HsvColor::new(0.0, 1.0, 1.0);
        let rgb = hsv.to_rgb();
        assert_eq!(rgb.r, 255);
        assert_eq!(rgb.g, 0);
        assert_eq!(rgb.b, 0);
    }

    #[test]
    fn hsv_to_rgb_green() {
        let hsv = HsvColor::new(1.0 / 3.0, 1.0, 1.0);
        let rgb = hsv.to_rgb();
        assert_eq!(rgb.r, 0);
        assert_eq!(rgb.g, 255);
        assert_eq!(rgb.b, 0);
    }

    #[test]
    fn hsv_to_rgb_blue() {
        let hsv = HsvColor::new(2.0 / 3.0, 1.0, 1.0);
        let rgb = hsv.to_rgb();
        assert_eq!(rgb.r, 0);
        assert_eq!(rgb.g, 0);
        assert_eq!(rgb.b, 255);
    }

    #[test]
    fn track_colors_are_distinct() {
        let color0 = generate_track_color_rgb(0, 0.85, 0.95);
        let color1 = generate_track_color_rgb(1, 0.85, 0.95);
        let color2 = generate_track_color_rgb(2, 0.85, 0.95);

        // Colors should be different from each other
        assert_ne!(color0.to_argb(), color1.to_argb());
        assert_ne!(color1.to_argb(), color2.to_argb());
        assert_ne!(color0.to_argb(), color2.to_argb());
    }

    #[test]
    fn track_color_is_consistent() {
        let color1 = generate_track_color_rgb(42, 0.85, 0.95);
        let color2 = generate_track_color_rgb(42, 0.85, 0.95);

        // Same track ID should produce same color
        assert_eq!(color1.to_argb(), color2.to_argb());
    }

    #[test]
    fn rgb_to_argb_preserves_values() {
        let rgb = RgbColor::new(128, 64, 192);
        let argb = rgb.to_argb();

        assert_eq!(argb, 0xFF8040C0);
    }
}
