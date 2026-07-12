// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
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
        gstpcap::plugin_register_static().unwrap();
    });
}

/// Build a complete legacy PCAP file (little-endian) from a global header,
/// a packet record header and packet data.
///
/// Global header (24 bytes):
///   magic: 0xa1b2c3d4 (native / LE)
///   version_major: 2
///   version_minor: 4
///   thiszone: 0
///   sigfigs: 0
///   snaplen: 0xffff
///   network: 1 (Ethernet)
fn build_pcap(ts_sec: u32, ts_usec: u32, caplen: u32, origlen: u32, packet_data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();

    // PCAP global header (24 bytes)
    buf.extend_from_slice(&0xa1b2c3d4u32.to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&4u16.to_le_bytes());
    buf.extend_from_slice(&0i32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0xffffu32.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());

    // Packet record header (16 bytes)
    buf.extend_from_slice(&ts_sec.to_le_bytes());
    buf.extend_from_slice(&ts_usec.to_le_bytes());
    buf.extend_from_slice(&caplen.to_le_bytes());
    buf.extend_from_slice(&origlen.to_le_bytes());

    buf.extend_from_slice(packet_data);

    buf
}

fn build_eth_ipv4_udp_packet(
    dst_mac: &[u8; 6],
    src_mac: &[u8; 6],
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::new();

    // Ethernet header (14 bytes)
    buf.extend_from_slice(dst_mac);
    buf.extend_from_slice(src_mac);
    buf.extend_from_slice(&0x0800u16.to_be_bytes()); // ethertype = IPv4

    // IPv4 header (20 bytes)
    let total_len = 20u16 + 8u16 + payload.len() as u16;
    // Version=4, IHL=5
    buf.push(0x45);
    buf.push(0x00); // DSCP + ECN
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&0x0000u16.to_be_bytes()); // ID
    buf.extend_from_slice(&0x4000u16.to_be_bytes()); // flags + fragment offset
    buf.push(0x32); // TTL
    buf.push(0x11); // protocol = UDP
    buf.extend_from_slice(&0x0000u16.to_be_bytes()); // checksum (0, we don't verify)
    buf.extend_from_slice(src_ip);
    buf.extend_from_slice(dst_ip);

    // UDP header (8 bytes)
    let udp_len = 8u16 + payload.len() as u16;
    buf.extend_from_slice(&src_port.to_be_bytes());
    buf.extend_from_slice(&dst_port.to_be_bytes());
    buf.extend_from_slice(&udp_len.to_be_bytes());
    buf.extend_from_slice(&0x0000u16.to_be_bytes()); // checksum

    buf.extend_from_slice(payload);

    buf
}

// Build a PCAPNG file with one SHB, one IDB, and a single Enhanced
// Packet Block. All blocks are little-endian.
fn build_pcapng(packet_data: &[u8], caplen: Option<u32>) -> Vec<u8> {
    let caplen = caplen.unwrap_or(packet_data.len() as u32);

    let mut buf = Vec::new();

    // Section Header Block (SHB)
    let shb_len: u32 = 28 + 4; // 28 bytes header + 4 bytes end-of-options
    buf.extend_from_slice(&0x0A0D0D0Au32.to_le_bytes()); // block_type
    buf.extend_from_slice(&shb_len.to_le_bytes()); // block_len1
    buf.extend_from_slice(&0x1A2B3C4Du32.to_le_bytes()); // BOM
    buf.extend_from_slice(&1u16.to_le_bytes()); // major_version
    buf.extend_from_slice(&0u16.to_le_bytes()); // minor_version
    buf.extend_from_slice(&(-1i64).to_le_bytes()); // section_len = -1
    // End of options
    buf.extend_from_slice(&0u16.to_le_bytes()); // option code
    buf.extend_from_slice(&0u16.to_le_bytes()); // option length
    buf.extend_from_slice(&shb_len.to_le_bytes()); // block_len2

    // Interface Description Block (IDB)
    let idb_len: u32 = 20 + 4; // 20 bytes header + 4 bytes end-of-options
    buf.extend_from_slice(&0x00000001u32.to_le_bytes()); // block_type
    buf.extend_from_slice(&idb_len.to_le_bytes()); // block_len1
    buf.extend_from_slice(&1u16.to_le_bytes()); // linktype = Ethernet
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&0u32.to_le_bytes()); // snaplen
    // End of options
    buf.extend_from_slice(&0u16.to_le_bytes()); // option code
    buf.extend_from_slice(&0u16.to_le_bytes()); // option length
    buf.extend_from_slice(&idb_len.to_le_bytes()); // block_len2

    // Enhanced Packet Block (EPB)
    let data_padded_len = (caplen as usize).div_ceil(4) * 4; // round up to 4
    // block_len1 = type(4) + len1(4) + if_id(4) + ts_high(4) + ts_low(4) + caplen(4) + origlen(4) + data_padded + end-of-options(4) + block_len2(4)
    let epb_total_len: u32 = 32 + data_padded_len as u32 + 4; // 32 = outer frame (8) + inner header (20) + block_len2 (4)

    buf.extend_from_slice(&0x00000006u32.to_le_bytes()); // block_type
    buf.extend_from_slice(&epb_total_len.to_le_bytes()); // block_len1
    buf.extend_from_slice(&0u32.to_le_bytes()); // interface_id = 0
    buf.extend_from_slice(&100u32.to_le_bytes()); // timestamp_high (seconds)
    buf.extend_from_slice(&0u32.to_le_bytes()); // timestamp_low
    buf.extend_from_slice(&caplen.to_le_bytes()); // caplen
    buf.extend_from_slice(&(packet_data.len() as u32).to_le_bytes()); // origlen

    // Packet data with padding
    buf.extend_from_slice(packet_data);
    let pad = data_padded_len - packet_data.len();
    buf.extend(std::iter::repeat_n(0u8, pad));

    // End of options (4 bytes: 2 bytes code + 2 bytes length, value is implicit)
    buf.extend_from_slice(&0u16.to_le_bytes()); // option code
    buf.extend_from_slice(&0u16.to_le_bytes()); // option length
    buf.extend_from_slice(&epb_total_len.to_le_bytes()); // block_len2

    buf
}

/// Build a PCAPNG file with one SHB, one IDB, and multiple EPBs.
fn build_pcapng_multi(packets: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Section Header Block (SHB)
    let shb_len: u32 = 28 + 4; // 28 bytes header + 4 bytes end-of-options
    buf.extend_from_slice(&0x0A0D0D0Au32.to_le_bytes());
    buf.extend_from_slice(&shb_len.to_le_bytes());
    buf.extend_from_slice(&0x1A2B3C4Du32.to_le_bytes()); // BOM
    buf.extend_from_slice(&1u16.to_le_bytes()); // major_version
    buf.extend_from_slice(&0u16.to_le_bytes()); // minor_version
    buf.extend_from_slice(&(-1i64).to_le_bytes()); // section_len
    buf.extend_from_slice(&0u16.to_le_bytes()); // opt code
    buf.extend_from_slice(&0u16.to_le_bytes()); // opt length
    buf.extend_from_slice(&shb_len.to_le_bytes()); // block_len2

    // Interface Description Block (IDB)
    let idb_len: u32 = 20 + 4;
    buf.extend_from_slice(&0x00000001u32.to_le_bytes());
    buf.extend_from_slice(&idb_len.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // linktype = Ethernet
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&0u32.to_le_bytes()); // snaplen
    buf.extend_from_slice(&0u16.to_le_bytes()); // opt code
    buf.extend_from_slice(&0u16.to_le_bytes()); // opt length
    buf.extend_from_slice(&idb_len.to_le_bytes()); // block_len2

    for (idx, packet_data) in packets.iter().enumerate() {
        let caplen = packet_data.len() as u32;
        let data_padded_len = (caplen as usize).div_ceil(4) * 4;
        let epb_total_len: u32 = 32 + data_padded_len as u32 + 4;

        buf.extend_from_slice(&0x00000006u32.to_le_bytes()); // block_type
        buf.extend_from_slice(&epb_total_len.to_le_bytes()); // block_len1
        buf.extend_from_slice(&0u32.to_le_bytes()); // interface_id
        buf.extend_from_slice(&(100u32 + idx as u32).to_le_bytes()); // ts_high
        buf.extend_from_slice(&0u32.to_le_bytes()); // ts_low
        buf.extend_from_slice(&caplen.to_le_bytes()); // caplen
        buf.extend_from_slice(&caplen.to_le_bytes()); // origlen

        // Packet data with padding
        buf.extend_from_slice(packet_data);
        let pad = data_padded_len - packet_data.len();
        buf.extend(std::iter::repeat_n(0u8, pad));

        // End of options
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&epb_total_len.to_le_bytes()); // block_len2
    }

    buf
}

const DEFAULT_SRC_MAC: [u8; 6] = [0x00, 0x0c, 0x29, 0xb2, 0x93, 0x7d];
const DEFAULT_DST_MAC: [u8; 6] = [0x00, 0x0c, 0x29, 0xa6, 0x13, 0x41];
const DEFAULT_SRC_IP: [u8; 4] = [10, 0, 0, 1];
const DEFAULT_DST_IP: [u8; 4] = [10, 0, 0, 2];
const DEFAULT_SRC_PORT: u16 = 1234;
const DEFAULT_DST_PORT: u16 = 5000;

const UDP_PAYLOAD: &[u8] = &[
    0x80, 0xe3, 0x7c, 0xca, 0x79, 0xba, 0x09, 0xc0, 0x70, 0x6e, 0x8b, 0x33, 0x05, 0x0a, 0x00, 0xa0,
];
const PCAP_CAPS_STR: &str = "raw/x-pcap";

#[test]
fn test_parse_pcap_eth_payload() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        UDP_PAYLOAD,
    );

    let pcap = build_pcap(100, 0, packet.len() as u32, packet.len() as u32, &packet);

    let buf = gst::Buffer::from_slice(pcap);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(
        out_buf.size(),
        UDP_PAYLOAD.len(),
        "Output buffer should contain only the UDP payload"
    );

    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_zerosize() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        &[],
    );

    let pcap = build_pcap(200, 0, packet.len() as u32, packet.len() as u32, &packet);

    let buf = gst::Buffer::from_slice(pcap);
    h.push(buf).unwrap();

    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_some(),
        "Should receive a zero-size buffer for zero-size UDP payload"
    );
    let out_buf = out_buf.unwrap();
    assert_eq!(
        out_buf.size(),
        0,
        "Output buffer should be 0 bytes for zero-size UDP payload"
    );
}

#[test]
fn test_parse_pcap_src_ip_filter() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("src-ip", "10.0.0.1");

    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        UDP_PAYLOAD,
    );
    let pcap = build_pcap(100, 0, packet.len() as u32, packet.len() as u32, &packet);

    let buf = gst::Buffer::from_slice(pcap);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(out_buf.size(), UDP_PAYLOAD.len());
    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcap_src_ip_filter_no_match() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("src-ip", "192.168.1.1");

    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        UDP_PAYLOAD,
    );
    let pcap = build_pcap(100, 0, packet.len() as u32, packet.len() as u32, &packet);

    let buf = gst::Buffer::from_slice(pcap);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(
        out_buf.is_none(),
        "No output should be produced when filter doesn't match"
    );
}

#[test]
fn test_parse_pcap_port_filter() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);

    h.element().unwrap().set_property("dst-port", 5000i32);
    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        UDP_PAYLOAD,
    );
    let pcap = build_pcap(100, 0, packet.len() as u32, packet.len() as u32, &packet);

    let buf = gst::Buffer::from_slice(pcap);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(out_buf.size(), UDP_PAYLOAD.len());
    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcapng_epb() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        UDP_PAYLOAD,
    );

    let pcapng = build_pcapng(&packet, None);

    let buf = gst::Buffer::from_slice(pcapng);
    h.push(buf).unwrap();

    let out_buf = h.pull().unwrap();
    assert_eq!(
        out_buf.size(),
        UDP_PAYLOAD.len(),
        "Output buffer should contain only the UDP payload from PCAPNG"
    );

    let map = out_buf.map_readable().unwrap();
    assert_eq!(&map[..], UDP_PAYLOAD);
}

#[test]
fn test_parse_pcapng_empty_payload() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let packet = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &DEFAULT_SRC_IP,
        &DEFAULT_DST_IP,
        DEFAULT_SRC_PORT,
        DEFAULT_DST_PORT,
        &[],
    );

    let pcapng = build_pcapng(&packet, None);

    let buf = gst::Buffer::from_slice(pcapng);
    h.push(buf).unwrap();
    h.push_event(gst::event::Eos::new());

    let out_buf = h.try_pull();
    assert!(out_buf.is_some(), "Should receive a zero-size buffer");
    assert_eq!(out_buf.unwrap().size(), 0);
}

#[test]
fn test_parse_pcapng_multiple_epbs() {
    init();

    let mut h = gst_check::Harness::new("pcapparse2");
    h.set_src_caps_str(PCAP_CAPS_STR);
    h.play();

    let payload1 = b"hello";
    let payload2 = b"world";

    let packet1 = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &[10, 0, 0, 1],
        &[10, 0, 0, 2],
        1000,
        2000,
        payload1,
    );

    let packet2 = build_eth_ipv4_udp_packet(
        &DEFAULT_DST_MAC,
        &DEFAULT_SRC_MAC,
        &[10, 0, 0, 3],
        &[10, 0, 0, 4],
        3000,
        4000,
        payload2,
    );

    let pcapng = build_pcapng_multi(&[&packet1, &packet2]);

    let buf = gst::Buffer::from_slice(pcapng);
    h.push(buf).unwrap();

    let out_buf1 = h.pull().unwrap();
    let map1 = out_buf1.map_readable().unwrap();
    assert_eq!(&map1[..], payload1);

    let out_buf2 = h.pull().unwrap();
    let map2 = out_buf2.map_readable().unwrap();
    assert_eq!(&map2[..], payload2);
}
