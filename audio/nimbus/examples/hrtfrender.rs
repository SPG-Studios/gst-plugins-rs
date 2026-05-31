// Copyright (C) 2021-2024 Tomasz Andrzejak <andreiltd@gmail.com>
// Copyright (C) 2026 Matthew Waters <matthew@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

use anyhow::Error;
use clap::Parser;

use std::sync::{Arc, Condvar, Mutex};
use std::{thread, time};

// Rotation in radians to apply to object position every 100 ms
const ROTATION: f32 = 2.0 / 180.0 * std::f32::consts::PI;

#[derive(Parser)]
#[command(author, version, about = "An example of HRTF elements usage")]
struct Cli {
    /// Sets file that contains HRTF filters
    #[arg(short, long, value_name = "SOFA")]
    sofa: Option<String>,
    /// Sets a WAV file
    uri: String,
}

// This example is compatible with SOFA files. If no SOFA file is provided a default is used.
//
// It is best to provide a mono input since this example involves moving a
// single object. If not, the input will be downmixed to mono using the
// audioconvert element.
//
// Running:
// - cargo run -p gst-plugin-hrtf --example hrtfrender -- --sofa my/sofa/file.sofa file:///awesome-mono.wav
//
// The example is placing an object in front of the listener at the beginning
// and then updates a position every 100ms to rotate an object clockwise. The
// `CoordinateSystem` of the spatial object is optional. If omitted, it is
// initialized by default to the `left-handed system`.
//
// In addition, the `sofalizer` has a global property for the coordinates system
// used in the SOFA file. All positions of the spatial objects will be converted
// to this system before being passed to the 'sofalizer' renderer.
//
// Note that if the `spatial-objects` property is omitted, the locations for
// rendering the input channels will be calculated from the input `Caps`, based
// on the input channel map. Therefore, if you have a 5.1 stream, for instance,
// the channels will be rendered in their canonical positions.
//
// For multi-channel inputs, it's also worth considering the addition of a
// limiter element downstream. This is because the rendered channels are
// downmixed to the output without any volume compensation.
fn run() -> Result<(), Error> {
    gst::init()?;
    gstaudionimbus::plugin_register_static()?;

    let cli = Cli::parse();
    let uri = cli.uri;

    let renderer = if let Some(sofa) = cli.sofa.as_ref() {
        // TODO: custom sofa
        format!("audionimbushrtfrender sofa={sofa}")
    } else {
        "audionimbushrtfrender".to_string()
    };

    let pipeline = gst::parse::launch(&format!(
        "uridecodebin uri={uri} ! audioconvert ! audio/x-raw,channels=1 !
            {renderer} name=hrtf ! audioresample ! autoaudiosink"
    ))?
    .downcast::<gst::Pipeline>()
    .expect("type error");

    let hrtf = pipeline.by_name("hrtf").expect("hrtf element not found");

    /*
    let objs = [gst::Structure::builder("application/spatial-object")
        .field("x", 0f32)
        .field("y", 0f32)
        .field("z", 1f32)
        .field("distance-gain", 1f32)
        .build()];

    hrtf.set_property("spatial-objects", gst::Array::new(objs));
    */

    let state_cond = Arc::new((Mutex::new(gst::State::Null), Condvar::new()));
    let state_cond_clone = Arc::clone(&state_cond);

    thread::spawn(move || {
        // Wait for the pipeline to start up
        {
            let (lock, cvar) = &*state_cond_clone;
            let mut state = lock.lock().unwrap();

            while *state != gst::State::Playing {
                state = cvar.wait(state).unwrap();
            }
        }

        loop {
            let x = hrtf.property::<f32>("pos-x");
            let z = hrtf.property::<f32>("pos-z");

            // rotate clockwise: https://en.wikipedia.org/wiki/Rotation_matrix
            let new_x = (x * f32::cos(ROTATION) + z * f32::sin(ROTATION)).clamp(-1.0, 1.0);
            let new_z = (-x * f32::sin(ROTATION) + z * f32::cos(ROTATION)).clamp(-1.0, 1.0);

            hrtf.set_property("pos-x", new_x);
            hrtf.set_property("pos-z", new_z);

            thread::sleep(time::Duration::from_millis(100));
        }
    });

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::StateChanged(state_changed)
                if state_changed.src().map(|s| s == &pipeline).unwrap_or(false)
                    && state_changed.current() == gst::State::Playing =>
            {
                let (lock, cvar) = &*state_cond;
                let mut state = lock.lock().unwrap();

                *state = gst::State::Playing;
                cvar.notify_one();
            }
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    msg.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}

fn main() {
    match run() {
        Ok(r) => r,
        Err(e) => eprintln!("Error! {e}"),
    }
}
