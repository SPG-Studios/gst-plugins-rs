//
// Copyright (C) 2026 Sanil Raut <sr1990003@gmail.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! H266 (VVC) RTP payloader/depayloader per RFC 9328.
//!
//! The shared NAL/FU/AP types live in [`common`]; the payloader and
//! depayloader elements live in [`pay`] and [`depay`].

mod common;
pub mod depay;
pub mod pay;

#[cfg(test)]
mod tests;
