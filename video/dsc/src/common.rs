// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::fmt;
use gst::glib;

/// Hash method enum shared between signer and verifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstDscHashMethod")]
pub enum HashMethod {
    #[enum_value(name = "SHA-1", nick = "sha1")]
    Sha1,
    #[enum_value(name = "SHA-224", nick = "sha224")]
    Sha224,
    #[enum_value(name = "SHA-256", nick = "sha256")]
    #[default]
    Sha256,
    #[enum_value(name = "SHA-384", nick = "sha384")]
    Sha384,
    #[enum_value(name = "SHA-512", nick = "sha512")]
    Sha512,
}

impl HashMethod {
    pub fn digest_size(self) -> usize {
        match self {
            HashMethod::Sha1 => 20,
            HashMethod::Sha224 => 28,
            HashMethod::Sha256 => 32,
            HashMethod::Sha384 => 48,
            HashMethod::Sha512 => 64,
        }
    }
}

impl fmt::Display for HashMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            HashMethod::Sha1 => "sha1",
            HashMethod::Sha224 => "sha224",
            HashMethod::Sha256 => "sha256",
            HashMethod::Sha384 => "sha384",
            HashMethod::Sha512 => "sha512",
        };
        write!(f, "{}", s)
    }
}

impl From<u8> for HashMethod {
    fn from(value: u8) -> Self {
        match value {
            0 => HashMethod::Sha1,
            1 => HashMethod::Sha224,
            2 => HashMethod::Sha256,
            3 => HashMethod::Sha384,
            4 => HashMethod::Sha512,
            _ => HashMethod::Sha256, // default
        }
    }
}

impl From<HashMethod> for u8 {
    fn from(method: HashMethod) -> Self {
        match method {
            HashMethod::Sha1 => 0,
            HashMethod::Sha224 => 1,
            HashMethod::Sha256 => 2,
            HashMethod::Sha384 => 3,
            HashMethod::Sha512 => 4,
        }
    }
}
