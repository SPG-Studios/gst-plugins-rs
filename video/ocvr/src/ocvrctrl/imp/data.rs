// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst_video::VideoFormat;

use super::CaptureRate;
use super::ContentRate;
use super::Method;
use super::SyncState;
use super::Tolerance;

// Runtime value storage
#[derive(Debug, Clone)]
pub struct Data {
    pub content_rate: Option<ContentRate>,
    pub capture_rate: Option<CaptureRate>,
    pub frame_format: Option<VideoFormat>,
    pub frame_size: (u32, u32),
    pub ping_window: Vec<u8>,
    pub pong_window: Vec<u8>,
    pub is_ping: bool,
    pub sync_state: SyncState,
    pub method: Method,
    pub retries: u32,
    pub upstream_caps: gst::caps::Caps,
}

#[derive(Debug, Clone)]
pub enum ResyncSolution {
    FuzzyCompare,
    Retry(u32),
    None,
}

impl Default for Data {
    fn default() -> Self {
        Data {
            content_rate: None,
            capture_rate: None,
            frame_format: None,
            frame_size: (0, 0),
            ping_window: vec![],
            pong_window: vec![],
            sync_state: SyncState::Idle,
            is_ping: true,
            method: Method::Auto,
            retries: 0,
            upstream_caps: gst::caps::Caps::new_empty(),
        }
    }
}

impl Data {
    pub fn reset(&mut self, rate: ContentRate, method: Method, retries: u32) {
        self.content_rate = match rate {
            ContentRate::Hint => None,
            _ => Some(rate),
        };
        self.capture_rate.take();
        self.frame_format.take();
        self.frame_size = (0, 0);
        self.reset_on_hint(method, retries);
    }

    pub fn reset_on_hint(&mut self, method: Method, retries: u32) {
        self.ping_window = vec![];
        self.pong_window = vec![];
        match self.content_rate {
            None => self.sync_state.reset(),
            Some(r) => {
                self.method_overwrite();
                self.sync_state = SyncState::sync(r);
            }
        }
        self.is_ping = true;
        self.method = method;
        self.retries = retries;
    }

    pub fn synced_caps(&self, drop: bool) -> gst::Caps {
        let r = match self.content_rate.unwrap() {
            ContentRate::Hz24 => 24,
            ContentRate::Hz30 => 30,
            ContentRate::Hz60 => 60 / (drop as i32 + 1),
            _ => unreachable!(),
        };

        let mut c = self.upstream_caps.clone();
        let s = c.make_mut().structure_mut(0).unwrap();
        s.set::<gst::Fraction>("framerate", gst::Fraction::new(r, 1));
        c
    }

    pub fn method_overwrite(&mut self) {
        if self.method != Method::Auto {
            return;
        }

        // in case of 60Hz content where fuzzy comparison might be
        // possible we have to choose fuzzy comparison otherwise we
        // may be stuck in the mismatch case forever
        if self.content_rate.unwrap() == ContentRate::Hz60 {
            self.method = Method::Fuzzy
        }
    }

    pub fn can_compare(&self) -> bool {
        !self.ping_window.is_empty() && self.ping_window.len() == self.pong_window.len()
    }

    pub fn on_synced(&mut self, retries: u32) {
        self.retries = retries;
    }

    pub fn on_sync_lost(
        &mut self,
        method: Method,
        retries: u32,
        tolerance: Tolerance,
    ) -> ResyncSolution {
        self.ping_window = vec![];
        self.pong_window = vec![];
        self.is_ping = true;

        match (self.method, self.retries, tolerance) {
            (Method::Auto, _, _) => {
                // in auto mode try again with fuzzy comparison
                self.method = Method::Fuzzy;
                self.sync_state.resync();
                ResyncSolution::FuzzyCompare
            }
            (_, _, Tolerance::Paranoid) | (_, 0, Tolerance::Strict) => {
                self.sync_state.reset();
                self.content_rate.take();
                self.method = method;
                self.retries = retries;
                ResyncSolution::None
            }
            (_, 0, Tolerance::Lazy) => {
                // endless retries
                self.retries = retries;
                self.sync_state.resync();
                ResyncSolution::Retry(self.retries)
            }
            (_, _, _) => {
                // retry
                self.retries -= 1;
                self.sync_state.resync();
                ResyncSolution::Retry(self.retries)
            }
        }
    }
}
