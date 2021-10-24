// Copyright (c) 2021 Emmanuel Gil Peyrot <linkmauve@linkmauve.fr>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use gst::prelude::*;
//use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gsthvif::plugin_register_static().expect("hvifdec test");
    });
}

#[test]
fn test_decode() {
    init();
    let data = include_bytes!("preferred.hvif").as_ref();
    let mut h = gst_check::Harness::new("hvifdec");

    h.set_src_caps_str("image/x-hvif");

    let buf = gst::Buffer::from_slice(data);
    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    h.push_event(gst::event::Eos::new());

    let mut expected_timestamp: Option<gst::ClockTime> = Some(gst::ClockTime::ZERO);
    let mut count = 0;
    let expected_duration: Option<gst::ClockTime> = Some(gst::ClockTime::from_seconds(1));

    while let Some(buf) = h.try_pull() {
        assert_eq!(buf.pts(), expected_timestamp);
        assert_eq!(buf.duration(), expected_duration);

        expected_timestamp = expected_timestamp.opt_add(expected_duration);
        count += 1;
    }

    assert_eq!(count, 1);

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");
    assert_eq!(
        caps,
        gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgra, 64, 64)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap()
    );
}
