// Copyright (c) 2021 Emmanuel Gil Peyrot <linkmauve@linkmauve.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use gst::prelude::*;

const DECODE_PIPELINE: &str =
    "rsfilesrc location=../../video/hvif/tests/preferred.hvif ! hvifdec ! autovideosink";

fn main() {
    gst::init().unwrap();
    gsthvif::plugin_register_static().expect("Failed to register hvif plugin");

    let pipeline = gst::parse_launch(DECODE_PIPELINE).unwrap();
    let bus = pipeline.bus().unwrap();

    pipeline
        .set_state(gst::State::Playing)
        .expect("Failed to set pipeline state to playing");

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(..) => break,
            MessageView::Error(err) => {
                println!(
                    "Error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            _ => (),
        }
    }
    pipeline
        .set_state(gst::State::Null)
        .expect("Failed to set pipeline state to null");
}
