// Copyright (C) 2025 Carlos Bentzen <cadubentzen@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstvvenc::plugin_register_static().expect("vvenc test");
    });
}

#[test]
fn test_encode_gray8() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_gray10() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray10Le16, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i420() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::I420, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i420_10() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::I42010le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_y42b() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Y42b, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i422_10() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::I42210le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_y444() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Y444, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

#[test]
fn test_encode_i444_10() {
    init();

    let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Y44410le, 160, 120)
        .fps((30, 1))
        .build()
        .unwrap();
    test_encode(&video_info);
}

fn test_encode(video_info: &gst_video::VideoInfo) {
    let mut h = gst_check::Harness::new("vvenc");
    {
        let vvenc = h.element().unwrap();
        vvenc.set_property("speed-preset", gstvvenc::enc::SpeedPreset::Faster);
    }
    h.play();
    h.set_src_caps(video_info.to_caps().unwrap());

    let fps = video_info.fps();
    let frame_duration = gst::ClockTime::from_seconds_f64(
        f64::try_from(fps.denom()).unwrap() / f64::try_from(fps.numer()).unwrap(),
    );

    let mut buffer = {
        let mut buffer = gst::Buffer::with_size(video_info.size()).unwrap();
        let buffer_ref = buffer.make_mut();
        buffer_ref.set_pts(gst::ClockTime::from_mseconds(0));
        let mut vframe = gst_video::VideoFrame::from_buffer_writable(buffer, video_info).unwrap();

        for v in vframe.plane_data_mut(0).unwrap() {
            *v = 0;
        }

        match video_info.format_info().depth()[0] {
            8 => {
                for v in vframe.plane_data_mut(0).unwrap() {
                    *v = 128;
                }

                if video_info.is_yuv() {
                    for v in vframe.plane_data_mut(2).unwrap() {
                        *v = 128;
                    }
                }
            }
            10 => {
                for v in vframe.plane_data_mut(0).unwrap().chunks_exact_mut(2) {
                    v[0] = 0;
                    v[1] = 2;
                }

                if video_info.is_yuv() {
                    for v in vframe.plane_data_mut(2).unwrap().chunks_exact_mut(2) {
                        v[0] = 0;
                        v[1] = 2;
                    }
                }
            }
            _ => unreachable!(),
        }

        vframe.into_buffer()
    };

    for _ in 0..5 {
        buffer = buffer.clone();
        let buffer_ref = buffer.make_mut();
        buffer_ref.set_pts(buffer_ref.pts().unwrap() + frame_duration);
        h.push(buffer.clone()).unwrap();
    }
    h.push_event(gst::event::Eos::new());

    for i in 0..5 {
        let buffer = h.pull().unwrap();
        if i == 0 {
            assert!(!buffer.flags().contains(gst::BufferFlags::DELTA_UNIT))
        }
    }
}
