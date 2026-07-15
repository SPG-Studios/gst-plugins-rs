// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

use super::picture_segment::{
    PICTURE_SEGMENT_PREFIX_SIZE, codestream_from_picture_segment, is_picture_segment,
};
use crate::tests::{
    ExpectedBuffer, ExpectedPacket, Source, run_test_pipeline, run_test_pipeline_and_validate_data,
};

const RTP_HEADER_SIZE: usize = 12;
// RFC 9134 JPEG XS RTP payload header size (`PayloadHeader::SIZE`).
const JXSV_PAYLOAD_HEADER_SIZE: usize = 4;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        crate::plugin_register_static().expect("rtpjxsv test");
    });
}

fn jxsc_caps() -> gst::Caps {
    gst::Caps::builder("image/x-jxsc")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("framerate", gst::Fraction::new(25, 1))
        .build()
}

fn jxsv_caps() -> gst::Caps {
    gst::Caps::builder("video/x-jxsv")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("framerate", gst::Fraction::new(25, 1))
        .build()
}

fn make_codestream(mut payload: Vec<u8>) -> Vec<u8> {
    payload[0] = 0xff;
    payload[1] = 0x10;
    payload
}

fn make_buffer(data: Vec<u8>, pts_ms: u64) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(gst::ClockTime::from_mseconds(pts_ms));
    }
    buffer
}

#[test]
fn test_jxsv_single_packet_frame() {
    init();

    let frame_data = make_codestream(vec![0xAB; 512]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + picture_segment_len)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(frame_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_multi_packet_frame() {
    init();

    let frame_data = make_codestream(vec![0xCD; 3000]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let payload_size = 1400 - RTP_HEADER_SIZE - JXSV_PAYLOAD_HEADER_SIZE;
    let last_payload_size = picture_segment_len - 2 * payload_size;
    assert!(last_payload_size <= payload_size);

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT)
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::empty())
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + last_payload_size)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(frame_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_two_frames() {
    init();

    let frame1 = make_codestream(vec![0x11; 800]);
    let frame2 = make_codestream(vec![0x22; 900]);
    let frame1_len = frame1.len();
    let frame2_len = frame2.len();
    let frame1_picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame1_len;
    let frame2_picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame2_len;
    let src = Source::Buffers(
        jxsc_caps(),
        vec![make_buffer(frame1, 0), make_buffer(frame2, 40)],
    );

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(0)
                .marker_bit(true)
                .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + frame1_picture_segment_len)
                .build(),
        ],
        vec![
            ExpectedPacket::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .flags(gst::BufferFlags::MARKER)
                .pt(96)
                .rtp_time(3600)
                .marker_bit(true)
                .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + frame2_picture_segment_len)
                .build(),
        ],
    ];

    let expected_depay = vec![
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(0))
                .size(frame1_len)
                .flags(gst::BufferFlags::DISCONT)
                .build(),
        ],
        vec![
            ExpectedBuffer::builder()
                .pts(gst::ClockTime::from_mseconds(40))
                .size(frame2_len)
                .flags(gst::BufferFlags::empty())
                .build(),
        ],
    ];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_packet_loss() {
    init();

    let frame_data = make_codestream(vec![0xEF; 3000]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let payload_size = 1400 - RTP_HEADER_SIZE - JXSV_PAYLOAD_HEADER_SIZE;
    let last_payload_size = picture_segment_len - 2 * payload_size;
    assert!(last_payload_size <= payload_size);

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT)
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .drop(true)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::empty())
            .pt(96)
            .rtp_time(0)
            .marker_bit(false)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + payload_size)
            .build(),
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + last_payload_size)
            .build(),
    ]];

    let expected_depay: Vec<Vec<ExpectedBuffer>> = vec![];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_picture_segment_passthrough() {
    init();

    let codestream = make_codestream(vec![0x33; 400]);
    let codestream_len = codestream.len();
    let builder =
        super::picture_segment::PictureSegmentBuilder::from_caps(&jxsv_caps(), 0).unwrap();
    let picture_segment = builder.wrap_codestream(&codestream, None).unwrap().0;
    let picture_segment_len = picture_segment.len();

    let src = Source::Buffers(jxsv_caps(), vec![make_buffer(picture_segment, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + picture_segment_len)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(codestream_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}

#[test]
fn test_jxsv_depay_outputs_picture_segment_when_negotiated() {
    init();

    let frame_data = make_codestream(vec![0x44; 512]);
    let expected_codestream = frame_data.clone();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_data.len();
    let src = Source::Buffers(jxsc_caps(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay ! video/x-jxsv";

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + picture_segment_len)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(picture_segment_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline_and_validate_data(
        src,
        pay,
        depay,
        expected_pay,
        expected_depay,
        move |data, _, _| {
            assert!(is_picture_segment(data));
            assert_eq!(
                codestream_from_picture_segment(data)?,
                expected_codestream.as_slice()
            );
            Ok(())
        },
    );
}

fn jxsc_caps_with_profile() -> gst::Caps {
    gst::Caps::builder("image/x-jxsc")
        .field("alignment", "frame")
        .field("interlace-mode", "progressive")
        .field("width", 640i32)
        .field("height", 480i32)
        .field("depth", 10i32)
        .field("sampling", "YCbCr-4:2:2")
        .field("framerate", gst::Fraction::new(25, 1))
        .field("profile", "Main422.10")
        .field("level", "1k-1")
        .field("sublevel", "Full")
        .build()
}

#[test]
fn test_jxsv_profile_level_sublevel_roundtrip() {
    init();

    let frame_data = make_codestream(vec![0xAB; 512]);
    let frame_len = frame_data.len();
    let picture_segment_len = PICTURE_SEGMENT_PREFIX_SIZE + frame_len;
    let src = Source::Buffers(jxsc_caps_with_profile(), vec![make_buffer(frame_data, 0)]);

    let pay = "rtpjxsvpay mtu=1400";
    let depay = "rtpjxsvdepay";

    let expected_pay = vec![vec![
        ExpectedPacket::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
            .pt(96)
            .rtp_time(0)
            .marker_bit(true)
            .size(RTP_HEADER_SIZE + JXSV_PAYLOAD_HEADER_SIZE + picture_segment_len)
            .build(),
    ]];

    let expected_depay = vec![vec![
        ExpectedBuffer::builder()
            .pts(gst::ClockTime::from_mseconds(0))
            .size(frame_len)
            .flags(gst::BufferFlags::DISCONT)
            .build(),
    ]];

    run_test_pipeline(src, pay, depay, expected_pay, expected_depay);
}
