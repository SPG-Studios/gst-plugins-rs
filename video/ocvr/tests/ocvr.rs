// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: Apache-2.0 or MIT

use gst::prelude::*;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

struct TestSetup<'a> {
    pub frames: usize,
    pub dups: [usize; 2],
    pub rate: i32,
    pub prop: &'a str,
}

#[derive(PartialEq)]
enum RateMatch {
    None,
    Hz24,
    Hz24_30,
    Hz24_30_60,
}

const HZ24_IN_60: TestSetup = TestSetup {
    frames: 2 * 60 / (2 + 3), // we make 5 frames out of 2
    dups: [2, 3],
    rate: 24,
    prop: "24Hz",
};

const HZ30_IN_60: TestSetup = TestSetup {
    frames: 60 / 2, // we make 2 frames out of 1
    dups: [2, 2],
    rate: 30,
    prop: "30Hz",
};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstocvr::plugin_register_static().expect("ocvr test");
    });
}

#[test]
fn test_detect_24_in_60() {
    run_ctrl_test(HZ24_IN_60);
}

#[test]
fn test_detect_30_in_60() {
    run_ctrl_test(HZ30_IN_60);
}

#[test]
fn test_hint() {
    run_hint_test();
}

fn run_ctrl_test(setup: TestSetup) {
    init();

    let bin = gst::parse_bin_from_description(
        &format!("videotestsrc pattern=ball num-buffers={:?} ! capsfilter name=filter caps=\"video/x-raw,width=(int)800,height=(int)480,format=(string)NV12,framerate=(fraction)60/1,interlace-mode=(string)progressive\"", setup.frames), false).unwrap();

    let srcpad = bin.by_name("filter").unwrap().static_pad("src").unwrap();
    let _ = bin.add_pad(&gst::GhostPad::with_target(Some("src"), &srcpad).unwrap());
    let mut g = gst_check::Harness::with_element(&bin, None, Some("src"));
    g.play();

    // set our expected output framerate
    let mut h = gst_check::Harness::new("ocvrctrl");
    {
        let ctrl = h.element().unwrap();
        ctrl.set_property_from_str("content-rate", setup.prop);
    }

    h.play();
    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 800, 480)
        .fps((60, 1))
        .build()
        .unwrap();
    h.set_src_caps(video_info.to_caps().unwrap());

    for i in 0..setup.frames {
        let buf = g.pull().unwrap();
        if (i % 2) != 0 {
            for _ in 0..setup.dups[0] {
                h.push(buf.copy()).expect("failed to read buffer");
            }
        } else {
            for _ in 0..setup.dups[1] {
                h.push(buf.copy()).unwrap();
            }
        }
    }

    loop {
        match h.try_pull_event() {
            Some(e) => {
                if let gst::EventView::Caps(e) = e.view() {
                    let c = e.caps();
                    let r = c
                        .structure(0)
                        .unwrap()
                        .get::<gst::Fraction>("framerate")
                        .unwrap();
                    // if we find our expected output framerate we are done
                    if *r.round().numer() == setup.rate && *r.round().denom() == 1i32 {
                        break;
                    }
                }
            }
            None => unreachable!(),
        }
    }
    h.push_event(gst::event::Eos::new());
}

fn run_hint_test() {
    init();

    let input_path = {
        let mut r = PathBuf::new();
        r.push(env!("CARGO_MANIFEST_DIR"));
        r.push("tests");
        r.push("24-30-60_in_60");
        r.set_extension("mkv");
        r
    };
    let pipeline = gst::parse_launch(&format!("filesrc location={:?} ! matroskademux ! h265parse ! ocvrhint name=h window-size=1 ! fakesink", input_path)).unwrap();

    // add a PadProbe to monitor the custom upstream events with the
    // detected framerate
    let h = pipeline
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name("h")
        .unwrap();
    let p = h.static_pad("sink").unwrap();

    // catch the rate events and let us know if all rates have been
    // found once the pipeline is done
    let rm = Arc::new(Mutex::new(RateMatch::None));
    let data = Arc::clone(&rm);
    p.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_p, info| {
        let d = info.data.as_ref().unwrap();
        match d {
            gst::PadProbeData::Event(e) => {
                if let gst::EventView::CustomUpstream(ce) = e.view() {
                    let mut rm = data.lock().unwrap();
                    match ce.structure().unwrap().get::<u32>("rate").unwrap() {
                        24 => {
                            if *rm == RateMatch::None {
                                *rm = RateMatch::Hz24;
                            }
                            gst::PadProbeReturn::Drop
                        }
                        30 => {
                            if *rm == RateMatch::Hz24 {
                                *rm = RateMatch::Hz24_30;
                            }
                            gst::PadProbeReturn::Drop
                        }
                        60 => {
                            if *rm == RateMatch::Hz24_30 {
                                *rm = RateMatch::Hz24_30_60;
                            }
                            gst::PadProbeReturn::Remove
                        }
                        _ => gst::PadProbeReturn::Ok,
                    }
                } else {
                    gst::PadProbeReturn::Ok
                }
            }
            _ => gst::PadProbeReturn::Ok,
        }
    });

    let bus = pipeline.bus().unwrap();
    pipeline
        .set_state(gst::State::Playing)
        .expect("Unable to set the pipeline to the `Playing` state");

    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        if let gst::MessageView::Eos(..) = msg.view() {
            break;
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Unable to set the pipeline to the `Null` state");

    assert!(*rm.lock().unwrap() == RateMatch::Hz24_30_60);
}
