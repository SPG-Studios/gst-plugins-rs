// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst_video::VideoFormat;

use super::CaptureRate;
use super::ContentRate;
use super::Method;
use super::SyncState;

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
        }
    }
}

impl Data {
    pub fn reset(&mut self, method: Method, retries: u32) {
        self.content_rate.take();
        self.capture_rate.take();
        self.frame_format.take();
        self.frame_size = (0, 0);
        self.ping_window = vec![];
        self.pong_window = vec![];
        self.sync_state.reset();
        self.is_ping = true;
        self.method = method;
        self.retries = retries;
    }

    pub fn can_compare(&self) -> bool {
        !self.ping_window.is_empty() && self.ping_window.len() == self.pong_window.len()
    }

    pub fn on_synced(&mut self, method: Method, retries: u32) {
        self.method = method;
        self.retries = retries;
    }

    pub fn can_detect_sync_loss(&self) -> bool {
        match (self.capture_rate.unwrap(), self.sync_state) {
            (CaptureRate::HZ_60, SyncState::Hz60(_)) => false,
            _ => true,
        }
    }

    pub fn on_sync_lost(&mut self, method: Method, retries: u32) -> ResyncSolution {
        self.ping_window = vec![];
        self.pong_window = vec![];
        self.is_ping = true;

        if self.method == Method::Auto {
            // in auto mode try again with fuzzy comparison
            self.method = Method::Fuzzy;
            self.sync_state.resync();
            ResyncSolution::FuzzyCompare
        } else {
            if self.retries == 0 {
                // if we cannot retry we are lost
                self.sync_state.reset();
                self.content_rate.take();
                self.method = method;
                self.retries = retries;
                ResyncSolution::None
            } else {
                // retry
                self.retries -= 1;
                self.sync_state.resync();
                ResyncSolution::Retry(self.retries)
            }
        }
    }
}
