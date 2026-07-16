//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Shared H266 (VVC) RTP types and constants (RFC 9328).

mod aggr;
mod fu;
mod nal;

pub(crate) use aggr::*;
pub(crate) use fu::*;
pub(crate) use nal::*;

/// RTP clock rate for H266 video.
pub(crate) const CLOCK_RATE: i32 = 90_000;

/// H266 NAL unit header size (RFC 9328 §1.1.4).
pub(crate) const NAL_HEADER_SIZE: usize = 2;
/// FU header size (RFC 9328 §4.3.3).
pub(crate) const FU_HEADER_SIZE: usize = 1;

// RFC 9328 §4.3 RTP packet types.
pub(crate) const RTP_TYPE_AP: u8 = 28;
pub(crate) const RTP_TYPE_FU: u8 = 29;
