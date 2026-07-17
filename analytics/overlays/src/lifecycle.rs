// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEventKind {
    Eos,
    FlushStart,
    FlushStop,
}

pub fn lifecycle_event_kind(event: &gst::Event) -> Option<LifecycleEventKind> {
    use gst::EventView;

    match event.view() {
        EventView::Eos(..) => Some(LifecycleEventKind::Eos),
        EventView::FlushStart(..) => Some(LifecycleEventKind::FlushStart),
        EventView::FlushStop(..) => Some(LifecycleEventKind::FlushStop),
        _ => None,
    }
}

pub trait OverlayLifecycle {
    fn reset_runtime_state(&self);

    fn lifecycle_start(&self) -> Result<(), gst::ErrorMessage> {
        self.reset_runtime_state();
        Ok(())
    }

    fn lifecycle_stop(&self) -> Result<(), gst::ErrorMessage> {
        self.reset_runtime_state();
        Ok(())
    }
}
