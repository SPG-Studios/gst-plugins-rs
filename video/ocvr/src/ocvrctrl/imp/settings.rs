// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use super::CaptureRate;
use super::ContentRate;
use super::Method;
use super::Tolerance;

// Default values of properties
pub const DEFAULT_CONTENT_RATE: ContentRate = ContentRate::Hint;
pub const DEFAULT_CAPTURE_RATES: CaptureRate = CaptureRate::all();
pub const DEFAULT_METHOD: Method = Method::Auto;
pub const DEFAULT_THRESHOLD: u32 = 1;
pub const DEFAULT_TOLERANCE: Tolerance = Tolerance::Strict;
pub const DEFAULT_RETRIES: u32 = 3;
pub const DEFAULT_ROWS: u32 = 10;
pub const DEFAULT_DROP: bool = false;
pub const DEFAULT_SEND_CAPS: bool = true;

// Property value storage
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub content_rate: ContentRate,
    pub capture_rates: CaptureRate,
    pub method: Method,
    pub threshold: u32,
    pub tolerance: Tolerance,
    pub retries: u32,
    pub rows: u32,
    pub drop: bool,
    pub send_caps: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            content_rate: DEFAULT_CONTENT_RATE,
            capture_rates: DEFAULT_CAPTURE_RATES,
            method: DEFAULT_METHOD,
            threshold: DEFAULT_THRESHOLD,
            tolerance: DEFAULT_TOLERANCE,
            retries: DEFAULT_RETRIES,
            rows: DEFAULT_ROWS,
            drop: DEFAULT_DROP,
            send_caps: DEFAULT_SEND_CAPS,
        }
    }
}

impl Settings {
    pub fn in_hint_mode(&self) -> bool {
        self.content_rate == ContentRate::Hint
    }

    pub fn capture_rate(&self, rate: CaptureRate) -> bool {
        self.capture_rates.contains(rate)
    }
}
