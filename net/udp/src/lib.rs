// Copyright (C) 2024 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * plugin-rsudp:
 *
 * Since: plugins-rs-0.16.0
 */
mod net;
mod udpsrc;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    unsafe {
        use gst::glib::translate::ToGlibPtr;
        let ptr: *const gst::ffi::GstPlugin = plugin.to_glib_none().0;
        (*(ptr as *mut gst::ffi::GstObject)).flags |= 1 << 6;
    }

    udpsrc::register(plugin)?;

    Ok(())
}

gst::plugin_define!(
    rsudp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MPL",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
