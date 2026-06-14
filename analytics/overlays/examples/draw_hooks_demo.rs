// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Demonstrates host-injected pre/post draw hooks on the object-detection
//! overlay.
//!
//! A `videotestsrc` frame gets a fake detection box attached (via a pad probe),
//! so the overlay renders a built-in box+label. The host then registers:
//!   * a **pre** hook — a translucent cyan panel the built-in box draws *over*;
//!   * a **post** hook — the sample `BorderHook` plus a translucent magenta
//!     panel that draws *over* the built-in box, plus a watermark label.
//!
//! Run via gst-env so the core elements resolve, e.g.:
//!   gst-env.py --builddir build-local \
//!     cargo run -p gst-plugin-overlays --example draw_hooks_demo -- out.png
//!
//! The overlay element is constructed directly from this linked crate, so the
//! plugin .so must NOT be on GST_PLUGIN_PATH (otherwise its GType clashes).

use gst::glib;
use gst::prelude::*;
use gst_analytics::AnalyticsRelationMetaODExt;
use gstoverlays::DrawCommand;
use gstoverlays::hooks::{BorderHook, DrawHook, DrawHookContext, OverlayDrawHooksExt};

const WIDTH: i32 = 320;
const HEIGHT: i32 = 240;
// Built-in detection box; the pre/post panels overlap it to show z-order.
const BOX: (i32, i32, i32, i32) = (90, 70, 140, 100);

fn translucent_panel(x: i32, y: i32, argb: u32) -> DrawCommand {
    DrawCommand::Rectangle {
        x: x as f32,
        y: y as f32,
        width: 120.0,
        height: 90.0,
        rotation: 0.0,
        argb,
        filled: true,
    }
}

fn main() {
    gst::init().unwrap();
    // Register the overlay elements in-process so the factory below finds them
    // without loading the plugin .so (which would clash on the GType).
    gstoverlays::plugin_register_static().unwrap();

    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "hooks_demo.png".to_string());

    let pipeline = gst::Pipeline::new();

    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", 1i32)
        .build()
        .unwrap();
    let caps = gst_video::VideoCapsBuilder::new()
        .width(WIDTH)
        .height(HEIGHT)
        .format(gst_video::VideoFormat::Rgba)
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()
        .unwrap();

    let overlay = gst::ElementFactory::make("odoverlay")
        .property("render-enabled", true)
        .build()
        .unwrap();

    // Pre hook: translucent cyan panel. The built-in box draws OVER it.
    overlay.set_pre_draw_hook(|_ctx: &DrawHookContext| {
        vec![translucent_panel(BOX.0 - 30, BOX.1 - 20, 0x6600_FFFF)]
    });

    // Post hook: the library's sample BorderHook, plus a translucent magenta
    // panel that draws OVER the built-in box, plus a watermark label.
    overlay.set_post_draw_hook(|ctx: &DrawHookContext| {
        let mut cmds = BorderHook {
            argb: 0xFF00_FF00,
            inset: 6.0,
        }
        .draw(ctx);
        cmds.push(translucent_panel(BOX.0 + 40, BOX.1 + 30, 0x66FF_00FF));
        cmds.push(DrawCommand::Text {
            x: 10.0,
            y: (ctx.height - 8) as f32,
            text: "host draw hooks demo".to_string(),
            argb: 0xFFFF_FF00,
        });
        cmds
    });

    let convert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let enc = gst::ElementFactory::make("pngenc")
        .property("snapshot", true)
        .build()
        .unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", &out)
        .build()
        .unwrap();

    pipeline
        .add_many([&src, &capsfilter, &overlay, &convert, &enc, &sink])
        .unwrap();
    gst::Element::link_many([&src, &capsfilter, &overlay, &convert, &enc, &sink]).unwrap();

    // Attach a fake detection box to each buffer so the overlay has built-in
    // content for the hooks to sit under/over.
    let sinkpad = overlay.static_pad("sink").unwrap();
    sinkpad.add_probe(gst::PadProbeType::BUFFER, |_pad, info| {
        if let Some(gst::PadProbeData::Buffer(ref mut buffer)) = info.data {
            let buffer = buffer.make_mut();
            let mut meta = gst_analytics::AnalyticsRelationMeta::add(buffer);
            let _ = meta.add_od_mtd(
                glib::Quark::from_str("person"),
                BOX.0,
                BOX.1,
                BOX.2,
                BOX.3,
                0.95,
            );
        }
        gst::PadProbeReturn::Ok
    });

    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                eprintln!("pipeline error: {}", err.error());
                break;
            }
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
    println!("wrote {out}");
}
