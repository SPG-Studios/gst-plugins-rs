// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

/**
 * plugin-ocvr
 *
 * Since: plugins-rs-0.13.0
 */
use gst::glib;

mod ocvrctrl;
mod ocvrhint;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    ocvrhint::register(plugin)?;
    ocvrctrl::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    ocvr,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
