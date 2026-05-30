// Copyright (C) 2022-2023 François Laignel <fengalin@free.fr>
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
        gstsymphonia::plugin_register_static().expect("symphonia test");
    });
}

#[cfg(feature = "mp3")]
#[derive(PartialEq)]
enum OutFormat {
    ForceS16,
    SameAsIn,
}

#[cfg(feature = "flac")]
#[derive(PartialEq)]
enum StreamInfoOrigin {
    Caps,
    InBand,
}

#[cfg(feature = "mp3")]
#[test]
fn mp3dec_original_format() {
    run_mp3dec(gst_audio::AudioLayout::Interleaved, OutFormat::SameAsIn);
}

#[cfg(feature = "mp3")]
#[test]
fn mp3dec_force_s16() {
    run_mp3dec(gst_audio::AudioLayout::Interleaved, OutFormat::ForceS16);
}

#[cfg(all(feature = "mp3", feature = "allow-planar"))]
#[test]
fn mp3dec_planar() {
    run_mp3dec(gst_audio::AudioLayout::NonInterleaved, OutFormat::SameAsIn);
}

#[cfg(feature = "mp3")]
fn run_mp3dec(layout: gst_audio::AudioLayout, format: OutFormat) {
    let data = include_bytes!("test.mp3");
    let packet_sizes = [731, 182, 522];
    let offsets: Vec<(usize, usize)> = packet_sizes
        .iter()
        .scan(0, |offset, size| {
            let prev = *offset;
            *offset += size;
            Some((prev, *offset))
        })
        .collect();

    init();

    let mut h_sink_caps = gst_audio::AudioCapsBuilder::for_encoding("audio/x-raw").layout(layout);

    let (format, sample_size) = match format {
        OutFormat::ForceS16 => {
            let format = gst_audio::AUDIO_FORMAT_S16;
            h_sink_caps = h_sink_caps.format(format);
            (format, std::mem::size_of::<i16>())
        }
        OutFormat::SameAsIn => {
            // Test file format is F32.
            // don't change the harness sink pad caps
            (gst_audio::AUDIO_FORMAT_F32, std::mem::size_of::<f32>())
        }
    };

    let mut h = gst_check::Harness::new("symphoniamp3dec");
    h.set_sink_caps(h_sink_caps.build());
    h.play();

    let caps = gst_audio::AudioCapsBuilder::for_encoding("audio/mpeg")
        .field("mpegversion", 1i32)
        .field("layer", 3i32)
        .field("parsed", true)
        .build();
    h.set_src_caps(caps);

    for (start, end) in offsets.iter() {
        let buffer = gst::Buffer::from_slice(&data[*start..*end]);
        h.push(buffer).unwrap();
    }

    h.push_event(gst::event::Eos::new());

    const CHANNELS: usize = 2;
    const RATE: usize = 44_100;

    const DECODED_SAMPLES: usize = 1_152;
    for _ in offsets.iter() {
        let buffer = h.pull().unwrap();
        assert_eq!(buffer.size(), DECODED_SAMPLES * sample_size * CHANNELS);

        #[cfg(feature = "audio-meta")]
        {
            let audio_meta = buffer.meta::<gst_audio::AudioMeta>().unwrap();
            assert_eq!(audio_meta.samples(), DECODED_SAMPLES);
            assert_eq!(audio_meta.info().layout(), layout);
            if layout == gst_audio::AudioLayout::NonInterleaved {
                assert_eq!(audio_meta.offsets(), &[0, DECODED_SAMPLES * sample_size]);
            }
        }
    }

    let expected_caps = gst_audio::AudioCapsBuilder::for_encoding("audio/x-raw")
        .format(format)
        .layout(layout)
        .rate(RATE as i32)
        .channels(CHANNELS as i32)
        .fallback_channel_mask()
        .build();

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");

    assert_eq!(caps, expected_caps);
}

#[cfg(feature = "flac")]
#[test]
fn flacdec_caps_stream_info() {
    run_flacdec(StreamInfoOrigin::Caps);
}

#[cfg(feature = "flac")]
#[test]
fn flacdec_in_band_stream_info() {
    run_flacdec(StreamInfoOrigin::InBand);
}

#[cfg(feature = "flac")]
fn run_flacdec(stream_info_orig: StreamInfoOrigin) {
    let data = include_bytes!("test.flac");
    let packet_sizes = [4, 38, 74, 2058, 2061, 473];
    let offsets: Vec<(usize, usize)> = packet_sizes
        .iter()
        .scan(0, |offset, &size| {
            let prev = *offset;
            *offset += size;
            Some((prev, *offset))
        })
        .collect();
    let decoded_samples = [4608, 4608, 1024];

    init();

    let mut h = gst_check::Harness::new("symphoniaflacdec");
    h.play();

    let mut caps = gst_audio::AudioCapsBuilder::for_encoding("audio/x-flac")
        .field("framed", true)
        .rate(44_100i32)
        .channels(1i32);

    if stream_info_orig == StreamInfoOrigin::Caps {
        let stream_info: Vec<u8> = b"\x7FFLAC\x01\x00\x00\x02"
            .iter()
            .chain(data[offsets[0].0..offsets[0].1].iter())
            .chain(data[offsets[1].0..offsets[1].1].iter())
            .copied()
            .collect();
        caps = caps.field(
            "streamheader",
            gst::Array::new([
                gst::Buffer::from_slice(stream_info.into_boxed_slice()),
                gst::Buffer::from_slice(&data[offsets[2].0..offsets[2].1]),
            ]),
        );
    }

    h.set_src_caps(caps.build());

    for (start, end) in offsets.iter() {
        let buffer = gst::Buffer::from_slice(&data[*start..*end]);
        h.push(buffer).unwrap();
    }

    h.push_event(gst::event::Eos::new());

    for samples in decoded_samples {
        let buffer = h.pull().unwrap();
        assert_eq!(buffer.size(), samples * std::mem::size_of::<i16>());
    }

    let expected_caps = gst_audio::AudioCapsBuilder::for_encoding("audio/x-raw")
        .format(gst_audio::AUDIO_FORMAT_S16)
        .layout(gst_audio::AudioLayout::Interleaved)
        .rate(44_100i32)
        .channels(1i32)
        .build();

    let caps = h
        .sinkpad()
        .expect("harness has no sinkpad")
        .current_caps()
        .expect("pad has no caps");

    assert_eq!(caps, expected_caps);
}
