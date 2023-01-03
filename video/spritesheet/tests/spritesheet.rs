// Copyright (C) 2023 Daily.co <rajneesh@daily.co>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib::once_cell::sync::Lazy;
use gst::prelude::*;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "spritesheet-test",
        gst::DebugColorFlags::empty(),
        Some("spritesheet plugin test"),
    )
});

struct CueData {
    canvas_idx: i64,
    tile_x: u32,
    tile_y: u32,
    tile_w: u32,
    tile_h: u32,
}

macro_rules! try_or_pause {
    ($l:expr) => {
        match $l {
            Ok(v) => v,
            Err(err) => {
                eprintln!("Skipping Test: {:?}", err);
                return Ok(());
            }
        }
    };
}

macro_rules! try_create_element {
    ($l:expr, $n:expr) => {
        match gst::ElementFactory::find($l) {
            Some(factory) => factory.create().name($n).build().unwrap(),
            None => {
                eprintln!("Could not find {} ({}) plugin, skipping test", $l, $n);
                return Ok(());
            }
        }
    };
    ($l:expr) => {{
        let alias: String = format!("test_{}", $l);
        try_create_element!($l, <std::string::String as AsRef<str>>::as_ref(&alias))
    }};
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstspritesheet::plugin_register_static().expect("spritesheet test");
    });
}

#[test]
fn test_spritesheet_element_received_cue_data() -> Result<(), ()> {
    init();

    const NB_SKIP_FRAMES: u32 = 100;
    const TILE_W: u32 = 320;
    const TILE_H: u32 = 180;
    const TILE_ROWS: u32 = 2;
    const TILE_COLUMNS: u32 = 2;
    const NB_CANVAS: u32 = 4;
    // some multiple of skip-frames to be able to assert
    const BUFFER_NB: u32 = NB_CANVAS * (TILE_COLUMNS * TILE_ROWS) * NB_SKIP_FRAMES;

    let pipeline = gst::Pipeline::with_name("video_pipeline");

    let video_src = try_create_element!("videotestsrc");
    video_src.set_property("num-buffers", BUFFER_NB as i32);

    let spritesheet = gst::ElementFactory::make("spritesheet")
        .name("test_spritesheet")
        .build()
        .expect("Must be able to instantiate spritesheet");
    spritesheet.set_property("num-skip-frames", NB_SKIP_FRAMES as i64);
    spritesheet.set_property("tile-width", TILE_W);
    spritesheet.set_property("tile-height", TILE_H);
    spritesheet.set_property("num-rows", TILE_ROWS);
    spritesheet.set_property("num-columns", TILE_COLUMNS);

    let fakesink = try_create_element!("fakesink");

    try_or_pause!(pipeline.add_many([&video_src, &spritesheet, &fakesink,]));
    try_or_pause!(gst::Element::link_many([
        &video_src,
        &spritesheet,
        &fakesink
    ]));

    gst::info!(CAT, "set spritesheet pipeline to playing state");

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    let mut received_cue_data: Vec<CueData> = Vec::new();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Element(msg) => {
                if let Some(s) = msg.structure() {
                    if s.name().as_str() == "spritesheet-tile-data" {
                        received_cue_data.push(CueData {
                            canvas_idx: s.get::<i64>("canvas-idx").unwrap(),
                            tile_x: s.get::<u32>("tile-x").unwrap(),
                            tile_y: s.get::<u32>("tile-y").unwrap(),
                            tile_w: s.get::<u32>("tile-width").unwrap(),
                            tile_h: s.get::<u32>("tile-height").unwrap(),
                        });
                    }
                }
            }
            MessageView::Error(..) => unreachable!(),
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos);
    // number of cueData = total tiles = num_canvas * tile_per_canvas
    assert_eq!(
        received_cue_data.len() as u32,
        NB_CANVAS * TILE_ROWS * TILE_COLUMNS
    );
    let mut expected_x = 0;
    let mut expected_y = 0;
    for (i, cu) in received_cue_data.iter().enumerate() {
        // tile width and height match the settings
        assert_eq!(cu.tile_w, TILE_W);
        assert_eq!(cu.tile_h, TILE_H);
        // canvas index after each tile_rows * tile_columns
        let expected_canvas_idx: u32 = (i as u32) / (TILE_ROWS * TILE_COLUMNS);
        assert_eq!(cu.canvas_idx, expected_canvas_idx as i64);
        // tiles in raster order
        assert_eq!(cu.tile_x, expected_x);
        assert_eq!(cu.tile_y, expected_y);

        expected_x += TILE_W;
        if expected_x >= (TILE_W * TILE_COLUMNS) {
            expected_x = 0;
            expected_y += TILE_H;
        }
        if expected_y >= TILE_H * TILE_ROWS {
            expected_x = 0;
            expected_y = 0;
        }
    }

    Ok(())
}
