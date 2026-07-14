// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Tests for the auto-selecting overlay bins (odoverlaybin, keypointsoverlaybin,
//! segoverlaybin, overlaycompositorbin).

use gst::prelude::*;
use std::sync::Once;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlays::plugin_register_static().expect("overlays test");
    });
}

/// (bin factory, CPU child factory it should select for system-memory input).
const BINS: &[(&str, &str)] = &[
    ("odoverlaybin", "odoverlay"),
    ("keypointsoverlaybin", "keypointsoverlay"),
    ("segoverlaybin", "segoverlay"),
    ("overlaycompositorbin", "overlaycompositor"),
];

#[test]
fn all_bins_are_registered() {
    init();
    for (bin, _) in BINS {
        assert!(
            gst::ElementFactory::find(bin).is_some(),
            "factory {bin} should be registered"
        );
    }
}

#[test]
fn bins_advertise_system_memory_caps_on_both_pads() {
    init();
    for (bin, _) in BINS {
        let element = gst::ElementFactory::make(bin).build().unwrap();
        for pad in ["sink", "src"] {
            let templ = element
                .pad_template(pad)
                .unwrap_or_else(|| panic!("{bin} missing {pad} template"));
            let caps = templ.caps().to_string();
            assert!(
                caps.contains("video/x-raw"),
                "{bin} {pad} caps {caps} should offer system-memory video/x-raw"
            );
        }
    }
}

// When built with GL support the union template must also advertise GLMemory so
// a GL upstream can negotiate the GL path. (Selecting the GL child at runtime
// needs a real GL context and is exercised via examples/gl_overlay_save.rs.)
#[cfg(feature = "gl")]
#[test]
fn bins_advertise_gl_memory_caps_when_built_with_gl() {
    init();
    for (bin, _) in BINS {
        let element = gst::ElementFactory::make(bin).build().unwrap();
        let caps = element.pad_template("sink").unwrap().caps().to_string();
        assert!(
            caps.contains("memory:GLMemory"),
            "{bin} sink caps {caps} should offer memory:GLMemory in a GL build"
        );
    }
}

#[test]
fn od_bin_forwards_properties_before_a_child_exists() {
    init();
    let bin = gst::ElementFactory::make("odoverlaybin").build().unwrap();

    // Defaults mirror the plain element (draw-labels defaults true).
    assert!(bin.property::<bool>("draw-labels"));

    // Setting is cached and read back even though no child has been built yet.
    bin.set_property("draw-labels", false);
    assert!(!bin.property::<bool>("draw-labels"));

    bin.set_property("priority", 7i32);
    assert_eq!(bin.property::<i32>("priority"), 7);
}

/// Drive a system-memory pipeline through each bin and assert it selected the CPU
/// child (not the GL one).
#[test]
fn bins_select_cpu_child_on_system_memory_input() {
    init();
    for (bin, cpu_child) in BINS {
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers=3 ! video/x-raw,width=32,height=24,format=RGBA \
             ! {bin} name=ov ! fakesink"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        // Wait for preroll: the sink caps event (which triggers selection) has
        // flowed by the time the state change completes.
        let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(5));
        res.unwrap_or_else(|_| panic!("{bin}: pipeline failed to reach PLAYING"));

        let ov = pipeline
            .by_name("ov")
            .unwrap()
            .downcast::<gst::Bin>()
            .unwrap();
        let child_factories: Vec<String> = ov
            .iterate_elements()
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.factory().map(|f| f.name().to_string()))
            .collect();

        assert!(
            child_factories.iter().any(|f| f == cpu_child),
            "{bin} should have built the CPU child {cpu_child}, found {child_factories:?}"
        );

        pipeline.set_state(gst::State::Null).unwrap();
    }
}
