// Copyright (C) 2022 Jochen Henneberg <jh@henneberg-systemdesign.com>
//
// SPDX-License-Identifier: MPL-2.0

// The test videos can be generated with the script
// test-video-generator.sh.

use gst::prelude::*;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(PartialEq)]
enum RateMatch {
    None,
    // for 60hz capture rate
    Hz24,
    Hz24_30,
    Hz24_30_60,
    // for 50Hz capture rate
    Hz25,
    Hz25_30,
    Hz25_30_50,
}

struct TestSetup<'a> {
    pub dups: Vec<usize>,
    pub content_rate: i32,
    pub capture_rate: i32,
    pub prop: &'a str,
    pub frames: usize,
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstocvr::plugin_register_static().expect("ocvr test");
    });
}

// fix rounding error or odd/even drop offset
const FIX: i32 = -1;

#[test]
fn ctrl_25_in_50() {
    const SYNC_IN_PERIOD: i32 = 5 + 1;
    const CAPTURE_RATE: i32 = 50;
    const CONTENT_RATE: i32 = CAPTURE_RATE / 2;
    run_ctrl_test(TestSetup {
        dups: vec![2, 2, 2],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "25Hz",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) / 2 + FIX) as usize,
    });
}

#[test]
fn ctrl_25_auto_in_50() {
    // 5 matches + 1 initial reference frame
    const SYNC_IN_PERIOD: i32 = 5 + 1;
    const CAPTURE_RATE: i32 = 50;
    const CONTENT_RATE: i32 = CAPTURE_RATE / 2;
    run_ctrl_test(TestSetup {
        dups: vec![2, 2, 2],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "Auto",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) / 2 + FIX) as usize,
    });
}

#[test]
fn ctrl_30_in_50() {
    // 5 matches + 1 initial reference frame
    const SYNC_IN_PERIOD: i32 = 5 + 1;
    const CAPTURE_RATE: i32 = 50;
    const CONTENT_RATE: i32 = 30;
    run_ctrl_test(TestSetup {
        dups: vec![2, 2, 1],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "30Hz",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) * 3 / 5) as usize,
    });
}

#[test]
fn ctrl_30_auto_in_50() {
    // first try 25Hz then 30Hz
    const SYNC_IN_PERIOD: i32 = 8 + 1 + 5 + 1;
    const CAPTURE_RATE: i32 = 50;
    const CONTENT_RATE: i32 = 30;
    run_ctrl_test(TestSetup {
        dups: vec![2, 2, 1],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "Auto",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) * 3 / 5) as usize,
    });
}

#[test]
fn ctrl_50_in_50() {
    const CAPTURE_RATE: i32 = 50;
    run_ctrl_test(TestSetup {
        dups: vec![1, 1, 1],
        content_rate: CAPTURE_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "50Hz",
        frames: CAPTURE_RATE as usize,
    });
}

#[test]
fn ctrl_50_auto_in_50() {
    const CAPTURE_RATE: i32 = 50;
    run_ctrl_test(TestSetup {
        dups: vec![1, 1, 1],
        content_rate: CAPTURE_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "Auto",
        frames: CAPTURE_RATE as usize,
    });
}

#[test]
fn ctrl_24_in_60() {
    // 2 matches + 1 initial reference frame
    const SYNC_IN_PERIOD: i32 = 2 + 1;
    const CAPTURE_RATE: i32 = 60;
    const CONTENT_RATE: i32 = 24;
    run_ctrl_test(TestSetup {
        dups: vec![3, 2],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "24Hz",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) * 2 / 5) as usize,
    });
}

#[test]
fn ctrl_24_auto_in_60() {
    // 2 matches + 1 initial reference frame
    const SYNC_IN_PERIOD: i32 = 2 + 1;
    const CAPTURE_RATE: i32 = 60;
    const CONTENT_RATE: i32 = 24;
    run_ctrl_test(TestSetup {
        dups: vec![3, 2],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "Auto",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) * 2 / 5) as usize,
    });
}

#[test]
fn ctrl_30_in_60() {
    // 5 matches + 1 initial reference frame
    const SYNC_IN_PERIOD: i32 = 5 + 1;
    const CAPTURE_RATE: i32 = 60;
    const CONTENT_RATE: i32 = CAPTURE_RATE / 2;
    run_ctrl_test(TestSetup {
        dups: vec![2, 2, 2],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "30Hz",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) / 2 + FIX) as usize,
    });
}

#[test]
fn ctrl_30_auto_in_60() {
    // first try 24Hz then 30Hz with offset 1
    const SYNC_IN_PERIOD: i32 = 8 + 1 + 5 + 1 + 1;
    const CAPTURE_RATE: i32 = 60;
    const CONTENT_RATE: i32 = CAPTURE_RATE / 2;
    run_ctrl_test(TestSetup {
        dups: vec![2, 2, 2],
        content_rate: CONTENT_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "Auto",
        frames: (SYNC_IN_PERIOD + (CAPTURE_RATE - SYNC_IN_PERIOD) / 2 + FIX) as usize,
    });
}

#[test]
fn ctrl_60_in_60() {
    const CAPTURE_RATE: i32 = 60;
    run_ctrl_test(TestSetup {
        dups: vec![1, 1, 1],
        content_rate: CAPTURE_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "60Hz",
        frames: CAPTURE_RATE as usize,
    });
}

#[test]
fn ctrl_60_auto_in_60() {
    const CAPTURE_RATE: i32 = 60;
    run_ctrl_test(TestSetup {
        dups: vec![1, 1, 1],
        content_rate: CAPTURE_RATE,
        capture_rate: CAPTURE_RATE,
        prop: "Auto",
        frames: CAPTURE_RATE as usize,
    });
}

#[test]
fn hint_60() {
    run_hint_test("24-30-60_in_60", RateMatch::Hz24_30_60);
}

#[test]
fn hint_50() {
    run_hint_test("25-30-50_in_50", RateMatch::Hz25_30_50);
}

fn run_ctrl_test(setup: TestSetup) {
    init();

    // we need motion=sweep otherwise we will get duplicate frames
    // when ball debounces from wall which causes false positives
    let bin = gst::parse_bin_from_description(
        &format!("videotestsrc pattern=ball motion=sweep num-buffers={:?} ! capsfilter name=filter caps=\"video/x-raw,width=(int)800,height=(int)480,format=(string)NV12,framerate=(fraction){:?}/1,interlace-mode=(string)progressive\"", setup.capture_rate, setup.capture_rate), false).unwrap();

    let srcpad = bin.by_name("filter").unwrap().static_pad("src").unwrap();
    let _ = bin.add_pad(&gst::GhostPad::with_target(&srcpad).unwrap());
    let mut g = gst_check::Harness::with_element(&bin, None, Some("src"));
    g.play();

    // set our expected output framerate
    let mut h = gst_check::Harness::new("ocvrctrl");
    {
        let ctrl = h.element().unwrap();
        ctrl.set_property_from_str("content-rate", setup.prop);
        ctrl.set_property_from_str("tolerance", "Lazy");
        ctrl.set_property_from_str("method", "Accurate");
    }

    h.play();
    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Nv12, 800, 480)
        .fps((setup.capture_rate, 1))
        .build()
        .unwrap();
    h.set_src_caps(video_info.to_caps().unwrap());

    for i in 0..setup.content_rate {
        let buf = g.pull().unwrap();
        for _ in 0..setup.dups[i as usize % setup.dups.len()] {
            h.push(buf.copy()).expect("failed to read buffer");
        }
    }

    let mut frames_found = 0;
    while h.try_pull().is_some() {
        frames_found += 1;
    }
    println!(">= {} out of {} frames found", frames_found, setup.frames);
    assert!(frames_found == setup.frames);

    let mut target_rate_found = false;
    while !target_rate_found {
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
                    let numer = *r.round().numer();
                    assert!(*r.round().denom() == 1i32);
                    if numer == setup.content_rate {
                        target_rate_found = true;
                        break;
                    }
                    assert!(numer == setup.capture_rate);
                }
            }
            None => break,
        }
    }
    h.push_event(gst::event::Eos::new());

    assert!(target_rate_found);
}

fn run_hint_test(f: &str, rate_match: RateMatch) {
    init();

    // the test file shall contain video material with changing
    // content framerates, for 60Hz video we should have 24Hz, 30Hz
    // and 60Hz and for 50Hz video we should have 25Hz, 30Hz and 50Hz
    let input_path = {
        let mut r = PathBuf::new();
        r.push(env!("CARGO_MANIFEST_DIR"));
        r.push("tests");
        r.push(f);
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
                        25 => {
                            if *rm == RateMatch::None {
                                *rm = RateMatch::Hz25;
                            }
                            gst::PadProbeReturn::Drop
                        }
                        30 => {
                            if *rm == RateMatch::Hz24 {
                                *rm = RateMatch::Hz24_30;
                            } else if *rm == RateMatch::Hz25 {
                                *rm = RateMatch::Hz25_30;
                            }
                            gst::PadProbeReturn::Drop
                        }
                        50 => {
                            if *rm == RateMatch::Hz25_30 {
                                *rm = RateMatch::Hz25_30_50;
                            }
                            gst::PadProbeReturn::Remove
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

    assert!(*rm.lock().unwrap() == rate_match);
}
