// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use std::sync::LazyLock;
use std::sync::Mutex;

use pcap_parser::pcap::parse_pcap_header;
use pcap_parser::pcapng;
use pcap_parser::traits::PcapNGPacketBlock;
use pcap_parser::{Linktype, nom};

const MAGIC_HEADER_SIZE_IN_BYTES: usize = 4;
const PCAP_HEADER_SIZE_IN_BYTES: usize = 16;
const PCAPNG_MAGIC_HEADER: u32 = 0x0A0D0D0A;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "rspcapparse",
        gst::DebugColorFlags::empty(),
        Some("PCAP parser Element"),
    )
});

#[derive(Debug)]
struct Settings {
    src_ip: String,
    dst_ip: String,
    src_port: i32,
    dst_port: i32,
    ts_offset: i64,
    caps: Option<gst::Caps>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            src_ip: "".to_string(),
            dst_ip: "".to_string(),
            src_port: -1i32,
            dst_port: -1i32,
            ts_offset: -1i64,
            caps: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PcapFormat {
    #[default]
    Unknown,
    Legacy,
    Ng,
}

#[derive(Default)]
struct InterfaceInfo {
    linktype: Linktype,
    ts_resolution: u64,
    ts_offset: i64,
}

struct State {
    buffer: Vec<u8>,

    needs_endianness: bool,
    swap_endian: bool,
    nanosecond_timestamp: bool,

    format: PcapFormat,
    linktype: Linktype,
    if_infos: Vec<InterfaceInfo>,

    newsegment_sent: bool,
    first_packet: bool,

    cur_ts: Option<gst::ClockTime>,
    base_ts: Option<gst::ClockTime>,

    packets: Vec<(Vec<u8>, Option<gst::ClockTime>)>,
}

impl Default for State {
    fn default() -> Self {
        State {
            buffer: Vec::new(),
            needs_endianness: true,
            swap_endian: false,
            nanosecond_timestamp: false,
            format: PcapFormat::default(),
            linktype: Linktype::default(),
            if_infos: Vec::new(),
            newsegment_sent: false,
            first_packet: false,
            cur_ts: None,
            base_ts: None,
            packets: Vec::with_capacity(1024),
        }
    }
}

impl State {
    fn read_u32_from_buffer(&self, offset: usize) -> u32 {
        let buffer = self.buffer.as_slice();
        let bytes = [
            buffer[offset],
            buffer[offset + 1],
            buffer[offset + 2],
            buffer[offset + 3],
        ];

        if self.swap_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    }
}

pub struct PcapParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl PcapParse {
    fn reset(&self) {
        let mut state = self.state.lock().unwrap();

        state.needs_endianness = true;
        state.swap_endian = false;
        state.nanosecond_timestamp = false;
        state.newsegment_sent = false;
        state.first_packet = true;

        state.linktype = Linktype(0);
        state.format = PcapFormat::Unknown;
        state.cur_ts = gst::ClockTime::NONE;
        state.base_ts = gst::ClockTime::NONE;

        state.if_infos.clear();
        state.buffer.clear();
    }

    // Adapted from `gst_pcap_parse_scan_frame` in `gstpcapparse.c`.
    fn extract_payload<'a>(
        &self,
        packet_data: &'a [u8],
        linktype: Linktype,
        settings: &Settings,
    ) -> Option<&'a [u8]> {
        use etherparse::{NetSlice, SlicedPacket};

        let sliced = match linktype {
            pcap_parser::Linktype(1) => SlicedPacket::from_ethernet(packet_data).ok()?,
            pcap_parser::Linktype(113) => SlicedPacket::from_linux_sll(packet_data).ok()?,
            pcap_parser::Linktype(276) => {
                // Linux SLL2: first 2 bytes are ethertype, then 18 bytes header
                if packet_data.len() < 20 {
                    return None;
                }
                let ether_type = u16::from_be_bytes([packet_data[0], packet_data[1]]);
                // Accept both IPv4 and IPv6
                if ether_type != 0x0800 && ether_type != 0x86DD {
                    return None;
                }
                SlicedPacket::from_ether_type(etherparse::EtherType(ether_type), &packet_data[20..])
                    .ok()?
            }
            pcap_parser::Linktype(101) | pcap_parser::Linktype(228) => {
                // Raw IPv4
                SlicedPacket::from_ether_type(etherparse::ether_type::IPV4, packet_data).ok()?
            }
            pcap_parser::Linktype(229) => {
                // Raw IPv6
                SlicedPacket::from_ether_type(etherparse::ether_type::IPV6, packet_data).ok()?
            }
            _ => return None,
        };

        match sliced.net.as_ref()? {
            NetSlice::Ipv4(ipv4) => {
                // TODO: Support fragmented IPv4 packets
                if ipv4.header().more_fragments()
                    || ipv4.header().fragments_offset() != etherparse::IpFragOffset::ZERO
                {
                    return None;
                }

                if !settings.src_ip.is_empty()
                    && let Ok(addr) = settings.src_ip.parse::<std::net::Ipv4Addr>()
                    && ipv4.header().source_addr() != addr
                {
                    return None;
                }

                if !settings.dst_ip.is_empty()
                    && let Ok(addr) = settings.dst_ip.parse::<std::net::Ipv4Addr>()
                    && ipv4.header().destination_addr() != addr
                {
                    return None;
                }
            }
            NetSlice::Ipv6(ipv6) => {
                // TODO: Support fragmented IPv6 packets
                if ipv6.is_payload_fragmented() {
                    return None;
                }

                if !settings.src_ip.is_empty()
                    && let Ok(addr) = settings.src_ip.parse::<std::net::Ipv6Addr>()
                    && ipv6.header().source_addr() != addr
                {
                    return None;
                }

                if !settings.dst_ip.is_empty()
                    && let Ok(addr) = settings.dst_ip.parse::<std::net::Ipv6Addr>()
                    && ipv6.header().destination_addr() != addr
                {
                    return None;
                }
            }
            _ => (),
        }

        match sliced.transport.as_ref()? {
            etherparse::TransportSlice::Udp(udp) => {
                if settings.src_port >= 0 && udp.source_port() != settings.src_port as u16 {
                    return None;
                }

                if settings.dst_port >= 0 && udp.destination_port() != settings.dst_port as u16 {
                    return None;
                }

                Some(udp.payload())
            }
            etherparse::TransportSlice::Tcp(tcp) => {
                if settings.src_port >= 0 && tcp.source_port() != settings.src_port as u16 {
                    return None;
                }

                if settings.dst_port >= 0 && tcp.destination_port() != settings.dst_port as u16 {
                    return None;
                }

                Some(tcp.payload())
            }
            _ => None,
        }
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Ok(data) = buffer.map_readable() else {
            gst::element_imp_error!(
                self,
                gst::StreamError::Failed,
                ["Failed to map buffer readable"]
            );
            return Err(gst::FlowError::Error);
        };

        gst::trace!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        state.buffer.extend_from_slice(data.as_slice());
        drop(data);

        // Collect all parsed packets as (payload, timestamp) pairs
        let mut ts: Option<gst::ClockTime> = gst::ClockTime::NONE;

        loop {
            if state.format == PcapFormat::Unknown {
                if state.buffer.len() < MAGIC_HEADER_SIZE_IN_BYTES {
                    break;
                }

                if PCAPNG_MAGIC_HEADER == state.read_u32_from_buffer(0) {
                    state.format = PcapFormat::Ng;
                    gst::info!(CAT, obj = pad, "Detected PCAPNG format");
                } else {
                    match parse_pcap_header(&state.buffer) {
                        Ok((_, header)) => {
                            state.format = PcapFormat::Legacy;

                            state.swap_endian = header.is_bigendian();
                            state.nanosecond_timestamp = header.is_nanosecond_precision();
                            state.linktype = header.network;

                            gst::info!(
                                CAT,
                                obj = pad,
                                "Detected legacy PCAP format, Interface: {}",
                                state.linktype.to_string()
                            );

                            state.buffer.drain(..header.size());
                        }
                        Err(nom::Err::Incomplete(_)) => {
                            // Need more data
                            if state.buffer.len() < PCAP_HEADER_SIZE_IN_BYTES {
                                break;
                            }

                            gst::element_imp_error!(
                                self,
                                gst::StreamError::WrongType,
                                ["Invalid PCAP header"]
                            );

                            return Err(gst::FlowError::Error);
                        }
                        Err(err) => {
                            gst::error!(CAT, obj = pad, "Invalid PCAP header {err:?}");

                            gst::element_imp_error!(
                                self,
                                gst::StreamError::WrongType,
                                ["Invalid PCAP header"]
                            );

                            return Err(gst::FlowError::Error);
                        }
                    }
                }
            }

            match state.format {
                PcapFormat::Ng => {
                    if state.needs_endianness {
                        match pcapng::parse_sectionheaderblock(&state.buffer) {
                            Ok((remaining, shb)) => {
                                let big_endian = shb.big_endian();
                                let block_size = state.buffer.len() - remaining.len();

                                state.swap_endian = big_endian;
                                state.buffer.drain(..block_size);

                                state.needs_endianness = false;

                                // Starting a new section, clear known interfaces
                                state.if_infos.clear();

                                gst::debug!(
                                    CAT,
                                    obj = pad,
                                    "New PCAPNG section, big_endian: {}",
                                    big_endian
                                );
                            }
                            Err(nom::Err::Incomplete(_)) => break,
                            Err(err) => {
                                gst::error!(CAT, obj = pad, "PCAPNG SHB parse error: {err:?}");

                                // Skip invalid data
                                if !state.buffer.is_empty() {
                                    state.buffer.drain(..1);
                                }
                                continue;
                            }
                        }
                    }

                    if state.buffer.is_empty() {
                        break;
                    }

                    let parse_result = if state.swap_endian {
                        pcapng::parse_block_be(&state.buffer)
                    } else {
                        pcapng::parse_block_le(&state.buffer)
                    };

                    match parse_result {
                        Ok((remaining, block)) => {
                            let consumed = state.buffer.len() - remaining.len();

                            match block {
                                pcapng::Block::SectionHeader(shb) => {
                                    let big_endian = shb.big_endian();

                                    state.swap_endian = big_endian;
                                    state.if_infos.clear();

                                    gst::debug!(
                                        CAT,
                                        obj = pad,
                                        "New PCAPNG section, big_endian: {}",
                                        big_endian
                                    );
                                }
                                pcapng::Block::InterfaceDescription(idb) => {
                                    let linktype = idb.linktype;
                                    let ts_resolution = idb
                                        .ts_resolution()
                                        .unwrap_or(u64::from(gst::ClockTime::USECOND));
                                    let ts_offset = idb.ts_offset();

                                    state.if_infos.push(InterfaceInfo {
                                        linktype,
                                        ts_resolution,
                                        ts_offset,
                                    });

                                    gst::debug!(
                                        CAT,
                                        obj = pad,
                                        "PCAPNG Interface: {}, ts_resolution: {}, ts_offset: {}",
                                        linktype.to_string(),
                                        ts_resolution,
                                        ts_offset
                                    );
                                }
                                pcapng::Block::EnhancedPacket(epb) => {
                                    let if_id = epb.if_id as usize;
                                    let packet_data = epb.packet_data().to_vec();
                                    let ts_high = epb.ts_high;
                                    let ts_low = epb.ts_low;

                                    if if_id < state.if_infos.len() {
                                        let if_info = &state.if_infos[if_id];
                                        let (ts_sec, ts_frac) = pcapng::build_ts(
                                            ts_high,
                                            ts_low,
                                            if_info.ts_offset as u64,
                                            if_info.ts_resolution,
                                        );

                                        let ts_frac_nanos = (ts_frac as u64)
                                            * (u64::from(gst::ClockTime::SECOND)
                                                / if_info.ts_resolution);
                                        ts = Some(
                                            gst::ClockTime::from_seconds(ts_sec as u64)
                                                + gst::ClockTime::from_nseconds(ts_frac_nanos),
                                        );

                                        if let Some(payload) = self.extract_payload(
                                            &packet_data,
                                            if_info.linktype,
                                            &settings,
                                        ) {
                                            state.packets.push((payload.to_vec(), ts));
                                        }
                                    }
                                }
                                pcapng::Block::SimplePacket(spb) if !state.if_infos.is_empty() => {
                                    let linktype = state.if_infos[0].linktype;
                                    let blen = (spb.block_len1 - 16) as usize;
                                    let packet_data = spb.data[..blen.min(spb.data.len())].to_vec();

                                    if let Some(payload) =
                                        self.extract_payload(&packet_data, linktype, &settings)
                                    {
                                        state.packets.push((payload.to_vec(), ts));
                                    }
                                }
                                _ => {
                                    // Ignore other block types
                                }
                            }

                            state.buffer.drain(..consumed);
                        }
                        Err(nom::Err::Incomplete(_)) => break,
                        Err(err) => {
                            gst::error!(CAT, obj = pad, "PCAPNG parse error: {err:?}");

                            // Skip invalid data
                            if !state.buffer.is_empty() {
                                state.buffer.drain(..1);
                            }
                        }
                    }

                    if !state.buffer.is_empty() {
                        continue;
                    }
                }
                PcapFormat::Legacy => {
                    if state.buffer.len() < PCAP_HEADER_SIZE_IN_BYTES {
                        break;
                    }

                    let ts_sec = state.read_u32_from_buffer(0);
                    let ts_usec = state.read_u32_from_buffer(4);
                    let incl_len = state.read_u32_from_buffer(8);

                    if state.buffer.len() < PCAP_HEADER_SIZE_IN_BYTES + incl_len as usize {
                        break;
                    }

                    let ts_frac = if state.nanosecond_timestamp {
                        ts_usec as u64
                    } else {
                        (ts_usec as u64) * u64::from(gst::ClockTime::USECOND)
                    };

                    ts = Some(
                        gst::ClockTime::from_seconds(ts_sec as u64)
                            + gst::ClockTime::from_nseconds(ts_frac),
                    );
                    state.cur_ts = ts;

                    let packet_data = &state.buffer
                        [PCAP_HEADER_SIZE_IN_BYTES..PCAP_HEADER_SIZE_IN_BYTES + incl_len as usize]
                        .to_vec();

                    if let Some(payload) =
                        self.extract_payload(packet_data, state.linktype, &settings)
                    {
                        state.packets.push((payload.to_vec(), ts));
                    }

                    state
                        .buffer
                        .drain(..PCAP_HEADER_SIZE_IN_BYTES + incl_len as usize);
                }
                PcapFormat::Unknown => break,
            }
        }

        if state.packets.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        let packets = state.packets.drain(..).collect::<Vec<_>>();
        let mut buflist = gst::BufferList::new();
        let buflist_ref = buflist.get_mut().unwrap();

        for (payload, ts) in packets.into_iter() {
            let mut buf = gst::Buffer::from_slice(payload);
            {
                let buf_ref = buf.get_mut().unwrap();

                if let Some(ts) = state.cur_ts {
                    if state.base_ts.is_none() {
                        state.base_ts = Some(ts);
                    }

                    if settings.ts_offset >= 0
                        && let Some(base_ts) = state.base_ts
                    {
                        let mut cur_ts = ts - base_ts;
                        cur_ts += gst::ClockTime::from_nseconds(settings.ts_offset as u64);

                        state.cur_ts = Some(cur_ts);
                    }
                }
                if let Some(t) = ts {
                    buf_ref.set_dts(Some(t));
                }

                if !state.first_packet {
                    buf_ref.unset_flags(gst::BufferFlags::DISCONT);
                } else {
                    buf_ref.set_flags(gst::BufferFlags::DISCONT);
                    state.first_packet = true;
                }
            }

            buflist_ref.add(buf);
        }

        let src_caps = settings.caps.clone();
        drop(settings);

        if !state.newsegment_sent {
            let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
            segment.set_start(state.base_ts);

            let segment_evt = gst::event::Segment::new(&segment);

            state.newsegment_sent = true;
            drop(state);

            if let Some(caps) = &src_caps {
                self.srcpad.push_event(gst::event::Caps::new(caps));
            }
            self.srcpad.push_event(segment_evt);
        }

        if buflist.is_empty() {
            return Ok(gst::FlowSuccess::Ok);
        }

        self.srcpad.push_list(buflist)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        match event.view() {
            EventView::FlushStop(_) => {
                self.reset();
                // Push event down the pipeline so that other elements
                // stop flushing fall through.
                self.srcpad.push_event(event)
            }
            EventView::Segment(_) => {
                // Drop it, we'll replace it with our own
                true
            }
            _ => self.srcpad.push_event(event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PcapParse {
    const NAME: &'static str = "GstRsPcapParse";
    type Type = super::PcapParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                PcapParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |pcapparse| pcapparse.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                PcapParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |pcapparse| pcapparse.sink_event(pad, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ).build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
        }
    }
}

impl ObjectImpl for PcapParse {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj().add_pad(&self.sinkpad).unwrap();
        self.obj().add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("src-ip")
                    .nick("Source IP")
                    .blurb("Source IP to restrict to")
                    .build(),
                glib::ParamSpecString::builder("dst-ip")
                    .nick("Destination IP")
                    .blurb("Destination IP to restrict to")
                    .build(),
                glib::ParamSpecInt::builder("src-port")
                    .nick("Source Port")
                    .blurb("Source port to restrict to")
                    .minimum(-1i32)
                    .maximum(i16::MAX as i32)
                    .readwrite()
                    .build(),
                glib::ParamSpecInt::builder("dst-port")
                    .nick("Destination Port")
                    .blurb("Destination port to restrict to")
                    .minimum(-1i32)
                    .maximum(i16::MAX as i32)
                    .readwrite()
                    .build(),
                glib::ParamSpecInt64::builder("ts-offset")
                    .nick("Timestamp Offset")
                    .blurb(
                        "Relative timestamp offset (ns) to apply (-1 = use absolute packet time)",
                    )
                    .minimum(-1i64)
                    .maximum(i64::MAX)
                    .readwrite()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Caps>("caps")
                    .nick("Caps")
                    .blurb("The caps of the source pad")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "src-ip" => {
                settings.src_ip = value.get::<String>().expect("type checked upstream");
            }
            "dst-ip" => {
                settings.dst_ip = value.get::<String>().expect("type checked upstream");
            }
            "src-port" => {
                settings.src_port = value.get::<i32>().expect("type checked upstream");
            }
            "dst-port" => {
                settings.dst_port = value.get::<i32>().expect("type checked upstream");
            }
            "ts-offset" => {
                settings.ts_offset = value.get::<i64>().expect("type checked upstream");
            }
            "caps" => {
                settings.caps = value
                    .get::<Option<gst::Caps>>()
                    .expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "src-ip" => settings.src_ip.to_value(),
            "dst-ip" => settings.dst_ip.to_value(),
            "src-port" => settings.src_port.to_value(),
            "dst-port" => settings.dst_port.to_value(),
            "ts-offset" => settings.ts_offset.to_value(),
            "caps" => settings.caps.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for PcapParse {}

impl ElementImpl for PcapParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "PCAP Parser",
                "Generic",
                "Parses PCAP stream",
                "Sanchayan Maity <sanchayan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let caps = gst::Caps::builder("raw/x-pcap").build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        let ret = self.parent_change_state(transition);

        if transition == gst::StateChange::PausedToReady {
            self.reset();
        }

        ret
    }
}
