// Copyright (C) 2025, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
//
// Copyright (C) 2026, Sanchayan Maity <sanchayan@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Implements Media over QUIC (MoQ) as per the following specifications
//
// - https://www.ietf.org/archive/id/draft-ietf-moq-transport-14.html
// - https://www.ietf.org/archive/id/draft-ietf-moq-msf-00.html
// - https://www.ietf.org/archive/id/draft-ietf-moq-cmsf-00.html
//
// QUIC connection or WebTransport session is shared with `quinnquicsrc`
// or `quinnwtsrc` upstream. MoQ Control messages require a bi-directional
// stream. This bi-directional stream is the Control channel and all
// Control messages are exchanged on this stream. This bi-directional
// stream & all control messages are handled here.
//
// Actual media is always sent on a uni-directional stream by the
// MoQ relay which is handled by either the QUIC or WebTransport
// element upstream.
//
// Once the setup or Subscription is complete in MoQ speak, media
// will always be received on the uni-directional stream by the
// source upstream. Data received by the source will then be demuxed
// into MoQ tracks based on their subscribe IDs. We do not allow
// specifying these IDs, at least not yet.
//
// `moqmux` and `moqdemux` are not symmetric. `moqmux` uses the caps sink
// event to assemble the information required by the `.catalog` track. For
// this reason `moqmux`, contains `cmafmux` inside while `moqdemux` does
// not.

// TODO: See TODOs in `moqmux`.

use crate::quinnconnection::*;
use crate::quinnquicmeta::QuinnQuicMeta;
use crate::quinnquicquery::*;
use crate::reader::Reader;
use crate::utils::{CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG, RUNTIME, WaitError, wait};
use crate::writer::Writer;
use crate::{common::*, utils};
use bytes::{Buf, BytesMut};
use gst::{glib, prelude::*, subclass::prelude::*};
use moq_transport::data::{StreamHeader, SubgroupHeader, SubgroupObject, SubgroupObjectExt};
use moq_transport::{coding::*, *};
use std::collections::{HashMap, hash_map};
use std::{
    io,
    sync::{Arc, LazyLock, Mutex},
};
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    sync::oneshot,
    task::JoinHandle,
};
use url::Url;
use web_transport_quinn::Session;
use web_transport_quinn::proto::{ConnectRequest, ConnectResponse};

static CATALOG_SUBSCRIBE_ID: u64 = 0;
static CATALOG_TRACK_NAME: &str = ".catalog";
static DEFAULT_MOQ_SCHEME: &str = "moqt";

// Below defaults are for testing with moq-rs-ietf.
static DEFAULT_MOQ_TRACK_NAMESPACE: &str = "bbb";
static DEFAULT_MOQ_RELAY_ADDR: &str = "127.0.0.1";
static DEFAULT_MOQ_RELAY_PORT: u16 = 4443;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "moqdemux",
        gst::DebugColorFlags::empty(),
        Some("Media over QUIC Demuxer"),
    )
});

fn parse_catalog(buffer: &gst::Buffer) -> Result<moq_catalog::Root, gst::ErrorMessage> {
    let map = buffer.map_readable().map_err(|err| {
        gst::error_msg!(
            gst::ResourceError::Failed,
            ["Failed to map buffer as readable: {err:?}"]
        )
    })?;

    let catalog: moq_catalog::Root = serde_json::from_slice(map.as_slice()).map_err(|err| {
        gst::error_msg!(
            gst::ResourceError::Failed,
            ["Failed to parse Catalog: {err:?}"]
        )
    })?;

    Ok(catalog)
}

struct TrackReader {
    buffer: BytesMut,
    stream_id: u64,
    // Valid only after Header is parsed
    subscribe_id: Option<u64>,
    stream_header: Option<StreamHeader>,
    subgroup_header: Option<SubgroupHeader>,
    has_extension_headers: bool,
    pending_payload_length: usize,
    ts: Option<gst::ClockTime>,
}

impl TrackReader {
    fn new(stream_id: u64) -> Self {
        Self {
            buffer: BytesMut::with_capacity(1024),
            subscribe_id: None,
            stream_id,
            stream_header: None,
            subgroup_header: None,
            has_extension_headers: false,
            pending_payload_length: 0,
            ts: None,
        }
    }

    fn push_buffer(&mut self, mut buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        if buffer.size() == 0 {
            return Ok(gst::FlowSuccess::Ok);
        }

        self.ts = buffer.dts_or_pts();

        let buffer_mut = buffer.get_mut().ok_or(gst::FlowError::Error)?;
        let map = buffer_mut
            .map_readable()
            .map_err(|_| gst::FlowError::Error)?;
        let buffer_slice = map.as_slice();

        self.buffer.extend_from_slice(buffer_slice);

        gst::trace!(
            CAT,
            "Added buffer of {} bytes, current buffer size: {}",
            buffer_slice.len(),
            self.buffer.len()
        );

        // Try to parse the header right away
        if self.stream_header.is_none() || self.subgroup_header.is_none() {
            self.parse_header();
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn recv_group(&mut self) -> Option<gst::Buffer> {
        if self.buffer.is_empty() {
            return None;
        }

        if self.pending_payload_length != 0 && self.buffer.len() < self.pending_payload_length {
            // Accumulate Object
            gst::trace!(
                CAT,
                "Accumulating Object: {}, Current length: {}, Stream: {}",
                self.pending_payload_length,
                self.buffer.len(),
                self.stream_id
            );
            return None;
        }

        let mut buffer = BytesMut::new();

        loop {
            if self.pending_payload_length != 0 {
                gst::trace!(CAT, "Accumulated Object: {}", self.pending_payload_length);
                buffer.extend_from_slice(&self.buffer[..self.pending_payload_length]);
                self.buffer.advance(self.pending_payload_length);
                self.pending_payload_length = 0;
            }

            let mut cursor = io::Cursor::new(&self.buffer);

            let decode_result = if self.has_extension_headers {
                SubgroupObjectExt::decode(&mut cursor).map(|obj| obj.payload_length)
            } else {
                SubgroupObject::decode(&mut cursor).map(|obj| obj.payload_length)
            };

            match decode_result {
                Ok(payload_length) => {
                    self.buffer.advance(cursor.position() as usize);

                    if payload_length == 0 {
                        break;
                    }

                    if self.buffer.len() < payload_length {
                        self.pending_payload_length = payload_length;
                        break;
                    }

                    buffer.extend_from_slice(&self.buffer[..payload_length]);
                    self.buffer.advance(payload_length);
                }
                Err(DecodeError::More(required)) => {
                    gst::trace!(CAT, "Need more data {}", self.buffer.len() + required);
                    break;
                }
                Err(err) => {
                    gst::error!(CAT, "Error decoding object: {err}");
                    break;
                }
            }
        }

        (!buffer.is_empty()).then(|| {
            let mut buffer = gst::Buffer::from_mut_slice(buffer);
            {
                let buffer = buffer.make_mut();
                buffer.set_pts(self.ts);
            }
            buffer
        })
    }

    fn parse_header(&mut self) {
        if self.buffer.is_empty() {
            return;
        }

        if self.stream_header.is_none() {
            gst::trace!(
                CAT,
                "TrackReader parsing stream header for stream id: {}",
                self.stream_id
            );

            let mut cursor = io::Cursor::new(&self.buffer);

            match StreamHeader::decode(&mut cursor) {
                Ok(stream_header) => {
                    gst::trace!(CAT, "StreamHeader: {stream_header:?}");

                    if stream_header.header_type.is_fetch() {
                        unimplemented!("Stream type Fetch is not supported");
                    }

                    assert!(stream_header.header_type.is_subgroup());

                    self.has_extension_headers = stream_header.header_type.has_extension_headers();
                    if let Some(subgroup_header) = stream_header.subgroup_header.as_ref() {
                        self.subscribe_id = Some(subgroup_header.track_alias);
                        self.subgroup_header = Some(subgroup_header.clone());
                    }
                    self.stream_header = Some(stream_header);

                    self.buffer.advance(cursor.position() as usize);
                }
                Err(DecodeError::More(required)) => {
                    gst::trace!(CAT, "Need more data {}", self.buffer.len() + required);
                }
                Err(err) => {
                    gst::error!(
                        CAT,
                        "StreamHeader parsing error: {err} for stream id: {}",
                        self.stream_id
                    );
                }
            }
        }

        if let Some(stream_header) = &self.stream_header
            && self.subgroup_header.is_none()
        {
            let mut cursor = io::Cursor::new(&self.buffer);

            match SubgroupHeader::decode(stream_header.header_type, &mut cursor) {
                Ok(s) => {
                    self.subscribe_id = Some(s.track_alias);
                    self.subgroup_header = Some(s);
                    self.buffer.advance(cursor.position() as usize);
                }
                Err(DecodeError::More(required)) => {
                    gst::trace!(CAT, "Need more data {}", self.buffer.len() + required);
                }
                Err(err) => {
                    gst::error!(CAT, "Error decoding SubgroupHeader: {err}");
                }
            }
        }
    }

    fn decode(&mut self) -> Option<gst::Buffer> {
        if self.buffer.is_empty() {
            gst::trace!(
                CAT,
                "TrackReader decode, buffer empty for stream id: {}",
                self.stream_id
            );
            return None;
        }

        gst::trace!(CAT, "TrackReader decode for stream id: {}", self.stream_id);

        if self.stream_header.is_none() || self.subgroup_header.is_none() {
            self.parse_header();
            None
        } else {
            self.recv_group()
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InitTrackState {
    WaitingForCatalog,
    WaitingForInitTracks,
    Ready,
}

struct InitTrackReader {
    state: InitTrackState,

    moq_catalog: Option<moq_catalog::Root>,
    track_namespace: TrackNamespace,
    subscribe_id: u64,

    init_track_buffer: HashMap<u64, gst::Buffer>,
    track_to_init_track_map: HashMap<String, u64>,
    track_subscribe_ids: HashMap<u64, String>,
    track_caps: HashMap<u64, gst::Caps>,
    init_track_subscribe_ids: Vec<u64>,
}

impl InitTrackReader {
    fn new(track_namespace: TrackNamespace) -> Self {
        Self {
            state: InitTrackState::WaitingForCatalog,

            track_namespace,
            subscribe_id: CATALOG_SUBSCRIBE_ID + 1,

            moq_catalog: None,
            init_track_buffer: HashMap::new(),
            track_to_init_track_map: HashMap::new(),
            track_subscribe_ids: HashMap::new(),
            track_caps: HashMap::new(),
            init_track_subscribe_ids: Vec::new(),
        }
    }

    fn init_tracks_done(&self) -> bool {
        self.state == InitTrackState::Ready
    }

    fn build_init_track_msgs(&mut self) -> Vec<message::Message> {
        let tracks = &self.moq_catalog.as_ref().unwrap().tracks;
        let mut subscribe_msgs: Vec<message::Message> = Vec::new();
        let mut observed_tracks: Vec<String> = Vec::new();
        let mut last_subscribe_id: u64 = 0;

        for track in tracks {
            let init_track = track
                .init_track
                .as_ref()
                .expect("Init track should be present");
            let track_name = track.name.clone();

            if !observed_tracks.contains(init_track) {
                let subscribe_msg: message::Message = message::Subscribe {
                    id: self.subscribe_id,
                    track_namespace: self.track_namespace.clone(),
                    track_name: init_track.clone(),
                    subscriber_priority: 0,
                    group_order: message::GroupOrder::Publisher,
                    forward: false,
                    filter_type: message::FilterType::AbsoluteStart,
                    start_location: Some(Location {
                        group_id: 0,
                        object_id: 0,
                    }),
                    end_group_id: None,
                    params: Default::default(),
                }
                .into();

                subscribe_msgs.push(subscribe_msg);

                self.init_track_subscribe_ids.push(self.subscribe_id);
                last_subscribe_id = self.subscribe_id;
                self.subscribe_id += 1;

                observed_tracks.push(init_track.clone());
            }

            gst::debug!(
                CAT,
                "Init track: {}, subscribe id: {} for track: {}",
                init_track,
                last_subscribe_id,
                track_name
            );

            self.track_to_init_track_map
                .insert(track_name, last_subscribe_id);
        }

        subscribe_msgs
    }

    fn build_track_subscribe_msgs(&mut self) -> Vec<message::Message> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let tracks = &self.moq_catalog.as_ref().unwrap().tracks;
        let mut subscribe_msgs = Vec::new();

        for track in tracks {
            let codec_mime = track
                .selection_params
                .codec
                .clone()
                .expect("Expect codec mime to be valid");
            let caps = gst_pbutils::codec_utils_caps_from_mime_codec(&codec_mime)
                .expect("Expect caps from codec mime to be valid");

            let subscribe_msg: message::Message = message::Subscribe {
                id: self.subscribe_id,
                track_namespace: self.track_namespace.clone(),
                track_name: track.name.clone(),
                subscriber_priority: 0,
                group_order: message::GroupOrder::Ascending,
                forward: true,
                filter_type: message::FilterType::AbsoluteStart,
                start_location: Some(Location {
                    group_id: 0,
                    object_id: 0,
                }),
                end_group_id: None,
                params: Default::default(),
            }
            .into();

            gst::debug!(
                CAT,
                "Sending Subscribe for track {} with subscribe id: {}",
                track.name,
                self.subscribe_id
            );

            subscribe_msgs.push(subscribe_msg);

            let (codec_data, media_name) = {
                let codec_data = track
                    .init_data
                    .clone()
                    .expect("Expect codec_data to be valid");
                (
                    STANDARD.decode(codec_data).unwrap(),
                    caps.structure(0).unwrap().name(),
                )
            };

            let caps = gst::Caps::builder(media_name)
                .field("codec_data", gst::Buffer::from_mut_slice(codec_data))
                .build();

            self.track_subscribe_ids
                .insert(self.subscribe_id, track.name.clone());
            self.track_caps.insert(self.subscribe_id, caps);

            self.subscribe_id += 1;
        }

        subscribe_msgs
    }

    fn push(
        &mut self,
        subscribe_id: u64,
        buffer: gst::Buffer,
    ) -> Result<Option<Vec<message::Message>>, gst::ErrorMessage> {
        match self.state {
            InitTrackState::WaitingForCatalog => {
                if subscribe_id != CATALOG_SUBSCRIBE_ID {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Unexpected subscribe_id, was expecting Catalog"]
                    ));
                }

                match parse_catalog(&buffer) {
                    Ok(catalog) => {
                        gst::info!(CAT, "Parsed Catalog: {catalog:?}");

                        if let Some(ref p) = catalog.common_track_fields.packaging
                            && *p != moq_catalog::TrackPackaging::Cmaf
                        {
                            return Err(gst::error_msg!(
                                gst::ResourceError::Failed,
                                ["Only CMAF packaging is supported"]
                            ));
                        }

                        self.moq_catalog = Some(catalog);
                        self.state = InitTrackState::WaitingForInitTracks;
                    }
                    Err(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to parse catalog: {err:?}"]
                        ));
                    }
                }

                Ok(Some(self.build_init_track_msgs()))
            }
            InitTrackState::WaitingForInitTracks => {
                if self.init_track_subscribe_ids.contains(&subscribe_id) {
                    self.init_track_buffer.insert(subscribe_id, buffer);

                    gst::info!(
                        CAT,
                        "Got init track buffer with subscribe id: {subscribe_id}",
                    );

                    if self.init_track_subscribe_ids.len() == self.init_track_buffer.len() {
                        gst::info!(CAT, "Received all init tracks");
                        self.state = InitTrackState::Ready;

                        gst::debug!(CAT, "Subscribe to Tracks");

                        return Ok(Some(self.build_track_subscribe_msgs()));
                    }

                    Ok(None)
                } else {
                    // No more pushes expected
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Unexpected InitTrackReader state"]
                    ))
                }
            }
            InitTrackState::Ready => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Unexpected InitTrackReader state"]
            )),
        }
    }

    fn get_init_track_buffer(&self, subscribe_id: u64) -> gst::Buffer {
        let track_name = self.track_subscribe_ids.get(&subscribe_id).unwrap();
        let init_track_subscribe_id = self.track_to_init_track_map.get(track_name).unwrap();

        let init_track_buffer = self
            .init_track_buffer
            .get(init_track_subscribe_id)
            .expect("Expect Init track buffer to be valid here");

        gst::trace!(
            CAT,
            "Returning init track buffer for subscribe id: {}",
            subscribe_id
        );

        init_track_buffer.clone()
    }

    fn get_caps_for_track(&self, subscribe_id: u64) -> gst::Caps {
        self.track_caps
            .get(&subscribe_id)
            .cloned()
            .expect("Expect caps to be present for track")
    }
}

struct Started {
    session: Arc<Session>,
    ctrl_handler_quit: Option<oneshot::Sender<()>>,
    ctrl_handler: Option<JoinHandle<()>>,
    // Subscribe ID/Track -> Pad
    pad_map: HashMap<u64 /* Subscribe ID */, gst::Pad>,
    // Stream ID -> Track Reader
    track_map: HashMap<u64, TrackReader>,
    init_track_reader: InitTrackReader,

    stream_close_tx: Option<Sender<u64>>,
    stream_close_handler: Option<JoinHandle<()>>,

    stream_start_id: Option<String>,
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(channel) = self.stream_close_tx.take() {
            drop(channel);
        }

        if let Some(channel) = self.ctrl_handler_quit.take() {
            let _ = channel.send(());
        }

        self.session
            .close(CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG.as_bytes());

        if let Some(handle) = self.stream_close_handler.take() {
            handle.abort();
        }

        if let Some(handle) = self.ctrl_handler.take() {
            handle.abort();
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Default)]
enum State {
    #[default]
    Stopped,
    Started(Started),
}

#[derive(Debug)]
struct Settings {
    timeout: u32,
    namespace: TrackNamespace,
    url: Url,
}

impl Default for Settings {
    fn default() -> Self {
        let relay_url = Url::parse(
            format!(
                "{}://{}:{}/{}",
                DEFAULT_MOQ_SCHEME,
                DEFAULT_MOQ_RELAY_ADDR,
                DEFAULT_MOQ_RELAY_PORT,
                DEFAULT_MOQ_TRACK_NAMESPACE
            )
            .as_str(),
        )
        .unwrap();

        Settings {
            timeout: DEFAULT_TIMEOUT,
            namespace: TrackNamespace::from_utf8_path(DEFAULT_MOQ_TRACK_NAMESPACE),
            url: relay_url,
        }
    }
}

pub struct MoqDemux {
    canceller: Mutex<utils::Canceller>,
    settings: Mutex<Settings>,
    sinkpad: gst::Pad,
    state: Mutex<State>,
}

impl GstObjectImpl for MoqDemux {}

impl ElementImpl for MoqDemux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Media over QUIC Demultiplexer",
                "Source/Network/QUIC",
                "Demultiplexes tracks/objects/groups for Media over QUIC",
                "Sanchayan Maity <sanchayan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let audio_srcpad_template = gst::PadTemplate::new(
                "audio_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            let video_srcpad_template = gst::PadTemplate::new(
                "video_%u",
                gst::PadDirection::Src,
                gst::PadPresence::Sometimes,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![
                sink_pad_template,
                audio_srcpad_template,
                video_srcpad_template,
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let gst::StateChange::ReadyToPaused = transition {
            let settings = self.settings.lock().unwrap();
            let track_namespace = settings.namespace.clone();
            drop(settings);

            match self.setup() {
                Ok((session, (reader, writer))) => {
                    let (tx_quit, rx_quit): (oneshot::Sender<()>, oneshot::Receiver<()>) =
                        oneshot::channel();

                    let (stream_close_tx, stream_close_rx) = mpsc::channel::<u64>(16);

                    let self_ = self.ref_counted();
                    let ctrl_handler: JoinHandle<()> = RUNTIME.spawn({
                        let self_ = self_.clone();
                        async move {
                            self_.handle_ctrl_messages(reader, rx_quit).await;
                            gst::debug!(CAT, imp = self_, "Control handler task exit");
                        }
                    });

                    let self_ = self.ref_counted();
                    let stream_close_handler: JoinHandle<()> = RUNTIME.spawn({
                        let self_ = self_.clone();
                        async move {
                            self_.handle_stream_close(writer, stream_close_rx).await;
                            gst::debug!(CAT, imp = self_, "Stream close handler task exit");
                        }
                    });

                    let mut state = self.state.lock().unwrap();
                    *state = State::Started(Started {
                        session,
                        ctrl_handler: Some(ctrl_handler),
                        ctrl_handler_quit: Some(tx_quit),
                        pad_map: HashMap::new(),
                        track_map: HashMap::new(),
                        init_track_reader: InitTrackReader::new(track_namespace),
                        stream_close_handler: Some(stream_close_handler),
                        stream_close_tx: Some(stream_close_tx),
                        stream_start_id: None,
                    });

                    gst::info!(CAT, imp = self, "MoQ setup done");
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "MoQ setup failed: {err:?}");
                    return Err(gst::StateChangeError);
                }
            }
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::ReadyToNull {
            *self.state.lock().unwrap() = State::Stopped;
            gst::info!(CAT, imp = self, "Stopped");
        }

        Ok(ret)
    }
}

impl ObjectImpl for MoqDemux {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .add_pad(&self.sinkpad)
            .expect("Failed to add sink pad");
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout MoQ endpoint requests (0 = No timeout).")
                    .maximum(3600)
                    .default_value(DEFAULT_TIMEOUT)
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("url")
                    .nick("MoQ URL to subscribe")
                    .blurb("MoQ URL to subscribe")
                    .build(),
                glib::ParamSpecString::builder("namespace")
                    .nick("Track namespace")
                    .blurb("MoQ Track namespace to subscribe")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "timeout" => {
                settings.timeout = value.get().expect("type checked upstream");
            }
            "url" => {
                let url = value.get::<String>().expect("type checked upstream");
                match Url::parse(&url) {
                    Ok(u) => {
                        let scheme = u.scheme().to_string();
                        if scheme != "https" && scheme != "moqt" {
                            gst::element_imp_error!(
                                self,
                                gst::ResourceError::Failed,
                                ["MoQ URL scheme must be https or moqt"]
                            );
                        }

                        settings.url = u;
                    }
                    Err(err) => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Failed,
                            ["Failed to parse MoQ URL: {err:?}"]
                        );
                    }
                }
            }
            "namespace" => {
                settings.namespace = TrackNamespace::from_utf8_path(
                    value
                        .get::<String>()
                        .expect("type checked upstream")
                        .as_str(),
                );
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "timeout" => settings.timeout.to_value(),
            "url" => settings.url.to_string().to_value(),
            "namespace" => settings.namespace.to_string().to_value(),
            _ => unimplemented!(),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for MoqDemux {
    const NAME: &'static str = "GstMoqDemux";
    type Type = super::MoqDemux;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let sinkpad = gst::Pad::builder_from_template(&klass.pad_template("sink").unwrap())
            .chain_function(|_pad, parent, buffer| {
                MoqDemux::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |demux| demux.sink_chain(buffer),
                )
            })
            .event_function(|pad, parent, event| {
                MoqDemux::catch_panic_pad_function(
                    parent,
                    || false,
                    |demux| demux.sink_event(pad, event),
                )
            })
            .build();

        Self {
            canceller: Mutex::new(utils::Canceller::default()),
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            sinkpad,
        }
    }
}

impl MoqDemux {
    fn open_session(&self, connection: QuinnConnection) -> Arc<Session> {
        let settings = self.settings.lock().unwrap();
        let scheme = settings.url.scheme().to_string();
        let url = settings.url.clone();
        drop(settings);

        match scheme.as_str() {
            "https" => match connection {
                QuinnConnection::WebTransport(session) => session,
                QuinnConnection::Quic(_) => unreachable!(),
            },
            "moqt" => match connection {
                QuinnConnection::Quic(connection) => {
                    let connect_req = ConnectRequest::new(url);
                    let response = ConnectResponse::new(http::StatusCode::OK);
                    Arc::new(Session::raw(connection, connect_req, response))
                }
                QuinnConnection::WebTransport(_) => unreachable!(),
            },
            // We do not expect to be here as we verify the scheme
            // when setting the URL property.
            _ => unreachable!(),
        }
    }

    async fn send_setup(&self, session: Session) -> Result<(Reader, Writer), gst::ErrorMessage> {
        match session.open_bi().await {
            Ok((s, r)) => {
                let mut writer = Writer::new(s);
                let mut reader = Reader::new(r);

                let versions: setup::Versions = [setup::Version::DRAFT_14].into();
                let client = setup::Client {
                    versions,
                    params: Default::default(),
                };

                match writer.encode(&client).await {
                    Ok(()) => {
                        gst::info!(CAT, imp = self, "Session client SETUP message send");
                    }
                    Err(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to send Setup message: {err:?}"]
                        ));
                    }
                }

                gst::info!(CAT, imp = self, "Waiting for server SETUP message");

                match reader.decode::<setup::Server>().await {
                    Ok(Some(s)) => {
                        gst::info!(CAT, imp = self, "Session established: {s:?}");
                    }
                    Ok(None) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to receive server setup message"]
                        ));
                    }
                    Err(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to receive server setup message: {err:?}"]
                        ));
                    }
                }

                Ok((reader, writer))
            }
            Err(err) => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Control channel request failed: {err:?}"]
            )),
        }
    }

    async fn subscribe_catalog(
        &self,
        reader: &mut Reader,
        writer: &mut Writer,
        track_namespace: TrackNamespace,
    ) -> Result<(), gst::ErrorMessage> {
        let subscribe_msg: message::Message = message::Subscribe {
            id: CATALOG_SUBSCRIBE_ID,
            track_namespace,
            track_name: CATALOG_TRACK_NAME.to_string(),
            subscriber_priority: 0,
            group_order: message::GroupOrder::Ascending,
            forward: true,
            filter_type: message::FilterType::AbsoluteStart,
            start_location: Some(Location {
                group_id: 0,
                object_id: 0,
            }),
            end_group_id: None,
            params: Default::default(),
        }
        .into();

        match writer.encode(&subscribe_msg).await {
            Ok(()) => {
                gst::info!(
                    CAT,
                    imp = self,
                    "Catalog Subscribe message send {subscribe_msg:?}"
                );
            }
            Err(err) => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to send Catalog Subscribe message: {err:?}"]
                ));
            }
        }

        match reader.decode::<message::Message>().await {
            Ok(Some(s)) => {
                gst::debug!(CAT, imp = self, "Catalog Subscribe response: {s:?}");
                match s {
                    message::Message::SubscribeOk(s) => {
                        gst::info!(CAT, imp = self, "Catalog SubscribeOk, {s:?}");
                    }
                    message::Message::SubscribeError(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to Subscribe to Catalog: {err:?}"]
                        ));
                    }
                    m => {
                        gst::warning!(CAT, imp = self, "Catalog Unexpected response: {m:?}");
                        unimplemented!()
                    }
                }
            }
            Ok(None) => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to Subscribe to Catalog"]
                ));
            }
            Err(err) => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to Subscribe to Catalog: {err:?}"]
                ));
            }
        }

        Ok(())
    }

    // Request the QUIC connection or WebTransport session from upstream
    fn setup_shared_session(&self) -> Result<QuinnConnection, gst::ErrorMessage> {
        let sinkpad = self
            .obj()
            .static_pad("sink")
            .expect("Sink pad must be valid");

        gst::debug!(CAT, imp = self, "Requesting Quinn Connection Context");

        utils::setup_shared_session(self.obj().clone().into(), &sinkpad)
    }

    fn setup_moq_session(&self, session: &Session) -> Result<(Reader, Writer), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        match wait(&self.canceller, self.send_setup(session.clone()), timeout) {
            Ok(s) => match s {
                Ok(session) => Ok(session),
                Err(err) => Err(err),
            },
            Err(err) => match err {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp = self, "Session setup aborted");
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Session setup aborted"]
                    ))
                }
                WaitError::FutureError(err) => Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Session setup failed: {err:?}"]
                )),
            },
        }
    }

    fn setup_subscription(
        &self,
        reader: &mut Reader,
        writer: &mut Writer,
    ) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        let track_namespace = settings.namespace.clone();
        drop(settings);

        match wait(
            &self.canceller,
            self.subscribe_catalog(reader, writer, track_namespace),
            timeout,
        ) {
            Ok(s) => match s {
                Ok(session) => Ok(session),
                Err(err) => Err(err),
            },
            Err(err) => match err {
                WaitError::FutureAborted => {
                    gst::warning!(CAT, imp = self, "Session setup aborted");
                    Err(gst::error_msg!(
                        gst::ResourceError::Failed,
                        ["Session setup aborted"]
                    ))
                }
                WaitError::FutureError(err) => Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Session setup failed: {err:?}"]
                )),
            },
        }
    }

    fn setup(&self) -> Result<(Arc<Session>, (Reader, Writer)), gst::ErrorMessage> {
        let shared_session = self.setup_shared_session()?;
        let session = self.open_session(shared_session);
        let (mut reader, mut writer) = self.setup_moq_session(&session)?;
        self.setup_subscription(&mut reader, &mut writer)?;

        Ok((session, (reader, writer)))
    }

    async fn handle_ctrl_messages(&self, mut reader: Reader, mut receiver: oneshot::Receiver<()>) {
        loop {
            tokio::select! {
                biased;
                quit = &mut receiver => match quit {
                    Ok(_) => break,
                    Err(e) => {
                            gst::error!(CAT, imp = self, "Error in oneshot channel {e:?}");
                            break;
                    },
                },
                res = reader.decode::<message::Message>() => match res {
                    Ok(Some(s)) => {
                        match s {
                            message::Message::PublishDone(p) => {
                                gst::debug!(CAT, imp = self, "PublishDone {p:?}");

                                let send_eos = {
                                    let state = self.state.lock().unwrap();
                                    matches!(&*state, State::Started(s) if s.pad_map.contains_key(&p.id))
                                };

                                if send_eos {
                                    self.obj().send_event(gst::event::Eos::new());
                                }
                            }
                            message::Message::SubscribeOk(s) => {
                                gst::info!(CAT, imp = self, "SubscribeOk {s:?}");
                            }
                            message::Message::SubscribeError(s) => {
                                gst::element_imp_error!(
                                    self,
                                    gst::ResourceError::Failed,
                                    ["Subscribe error: {:?}", s.reason_phrase]
                                );
                            }
                            m => {
                                gst::warning!(CAT, imp = self, "Unhandled control message: {m:?}");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        gst::error!(CAT, imp = self, "Error receiving control messages: {err:?}");
                        break;
                    }
                },
            }
        }

        gst::info!(CAT, imp = self, "Quit control message handler thread");
    }

    fn create_srcpad(
        &self,
        stream_start_id: String,
        subscribe_id: u64,
        caps: gst::Caps,
        tracks_len: usize,
    ) -> gst::Pad {
        let s = caps.structure(0).unwrap();
        let is_video = s.name().contains("video");
        let pad_name = if is_video { "video_%u" } else { "audio_%u" };

        let templ = self.obj().element_class().pad_template(pad_name).unwrap();

        let stream_pad_name = if is_video {
            format!("video_{}", subscribe_id)
        } else {
            format!("audio_{}", subscribe_id)
        };

        let srcpad = gst::Pad::builder_from_template(&templ)
            .name(stream_pad_name.clone())
            .build();

        srcpad.set_active(true).unwrap();

        let stream_start_evt = gst::event::StreamStart::builder(&stream_start_id)
            .group_id(gst::GroupId::next())
            .build();
        srcpad.push_event(stream_start_evt);

        let segment_evt = gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new());
        srcpad.push_event(segment_evt);

        self.obj().add_pad(&srcpad).expect("Failed to add pad");

        let no_more_pads = tracks_len == self.obj().src_pads().len();
        if no_more_pads {
            gst::info!(CAT, imp = self, "No more pads");
            self.obj().no_more_pads();
        }

        gst::debug!(
            CAT,
            imp = self,
            "Added pad {stream_pad_name} for stream id {subscribe_id}"
        );

        srcpad
    }

    fn stream_sink_chain(
        &self,
        buffer: gst::Buffer,
        stream_id: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let State::Started(ref mut started) = *state else {
            return Ok(gst::FlowSuccess::Ok);
        };

        if let hash_map::Entry::Vacant(e) = started.track_map.entry(stream_id) {
            e.insert(TrackReader::new(stream_id));

            gst::trace!(
                CAT,
                imp = self,
                "Created new TrackReader for stream id: {stream_id}"
            );
        }

        let Some(reader) = started.track_map.get_mut(&stream_id) else {
            return Ok(gst::FlowSuccess::Ok);
        };

        reader.push_buffer(buffer)?;

        if reader.stream_header.is_none() || reader.subgroup_header.is_none() {
            // We could not even parse header with data so far,
            // just return
            return Ok(gst::FlowSuccess::Ok);
        }

        // Header is parsed, Subscribe ID should be valid
        let subscribe_id = reader.subscribe_id;

        gst::trace!(
            CAT,
            imp = self,
            "Stream id: {stream_id}, Subscribe id: {subscribe_id:?}"
        );

        if !started.init_track_reader.init_tracks_done() {
            gst::info!(CAT, imp = self, "Waiting to receive all init tracks");
            return Ok(gst::FlowSuccess::Ok);
        }

        let (Some(buffer), Some(subscribe_id)) = (reader.decode(), subscribe_id) else {
            return Ok(gst::FlowSuccess::Ok);
        };

        let caps = started.init_track_reader.get_caps_for_track(subscribe_id);

        let srcpad = match started.pad_map.get(&subscribe_id) {
            Some(pad) => pad.clone(),
            None => {
                let tracks_len = started
                    .init_track_reader
                    .moq_catalog
                    .as_ref()
                    .unwrap()
                    .tracks
                    .len();
                let init_track_buffer = started
                    .init_track_reader
                    .get_init_track_buffer(subscribe_id);
                let stream_start_id = started.stream_start_id.clone().unwrap();

                drop(state);

                let srcpad = self.create_srcpad(stream_start_id, subscribe_id, caps, tracks_len);

                state = {
                    let mut state = self.state.lock().unwrap();
                    let State::Started(ref mut started) = *state else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    started.pad_map.insert(subscribe_id, srcpad.clone());
                    state
                };

                gst::debug!(
                    CAT,
                    imp = self,
                    "Stream id: {stream_id}, Subscribe id: {subscribe_id}, pad_created {srcpad:?}"
                );

                srcpad.push(init_track_buffer)?;

                srcpad
            }
        };

        drop(state);

        gst::trace!(
            CAT,
            imp = self,
            "Pushing buffer for subscribe id: {}",
            subscribe_id
        );

        srcpad.push(buffer)
    }

    fn sink_chain(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        {
            let mut state = self.state.lock().unwrap();
            let State::Started(_) = &mut *state else {
                return Ok(gst::FlowSuccess::Ok);
            };
        }

        let Some(meta) = buffer.meta::<QuinnQuicMeta>() else {
            gst::warning!(CAT, imp = self, "Buffer dropped, no metadata");
            return Ok(gst::FlowSuccess::Ok);
        };

        if meta.is_datagram() {
            gst::trace!(CAT, imp = self, "Got buffer on datagram, dropping...");
            return Ok(gst::FlowSuccess::Ok);
        }

        let stream_id = meta.stream_id();
        gst::trace!(CAT, imp = self, "Got buffer on stream: {stream_id}");

        self.stream_sink_chain(buffer, stream_id)
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::trace!(CAT, imp = self, "Handling event {:?}", event);

        match event.view() {
            EventView::CustomDownstream(ev) => {
                let Some(s) = ev.structure() else {
                    gst::warning!(CAT, imp = self, "Missing Structure from {event:?}");
                    return false;
                };

                if s.name() != QUIC_STREAM_CLOSE_CUSTOMDOWNSTREAM_EVENT {
                    return true;
                }

                let Ok(stream_id) = s.get::<u64>(QUIC_STREAM_ID) else {
                    gst::warning!(CAT, imp = self, "Missing Stream ID");
                    return false;
                };

                gst::trace!(CAT, imp = self, "Stream closed: {stream_id}");

                if let Some(tx) = {
                    let state = self.state.lock().unwrap();
                    match &*state {
                        State::Started(state) => state.stream_close_tx.clone(),
                        _ => None,
                    }
                } {
                    RUNTIME.spawn(async move {
                        let _ = tx.send(stream_id).await;
                    });
                }

                true
            }
            // We will push our own StreamStart/Segment events
            EventView::StreamStart(ev) => {
                let mut state = self.state.lock().unwrap();
                let State::Started(ref mut started) = *state else {
                    return false;
                };
                started.stream_start_id = Some(ev.stream_id().to_owned());
                true
            }
            EventView::Segment(_) => true,
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    async fn handle_stream_close(&self, mut writer: Writer, mut stream_close_rx: Receiver<u64>) {
        loop {
            let Some(stream_id) = stream_close_rx.recv().await else {
                break;
            };

            let (s_msgs, buffer, subscribe_id) = {
                let mut state = self.state.lock().unwrap();
                let State::Started(ref mut state) = *state else {
                    continue;
                };

                gst::trace!(CAT, imp = self, "Handling close for Stream ID: {stream_id}");

                let Some(ref mut reader) = state.track_map.remove(&stream_id) else {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Missing TrackReader for Stream ID: {stream_id}"
                    );
                    continue;
                };

                let (Some(buffer), Some(subscribe_id)) = (reader.decode(), reader.subscribe_id)
                else {
                    continue;
                };

                if !state.init_track_reader.init_tracks_done() {
                    let s_msgs = state.init_track_reader.push(subscribe_id, buffer);
                    (s_msgs, None, subscribe_id)
                } else {
                    (Ok(None), Some(buffer), subscribe_id)
                }
            };

            match buffer {
                None => match s_msgs {
                    Ok(Some(subscribe_msgs)) => {
                        for subscribe_msg in subscribe_msgs {
                            if let Err(err) = writer.encode(&subscribe_msg).await {
                                gst::element_imp_error!(
                                    self,
                                    gst::ResourceError::Failed,
                                    ["Failed to send init track request: {err:?}"]
                                );
                                continue;
                            }
                        }
                    }
                    Ok(None) => {
                        gst::trace!(
                            CAT,
                            imp = self,
                            "Pushing buffer for subscribe id: {subscribe_id:?}",
                        );
                    }
                    Err(err) => {
                        gst::element_imp_error!(self, gst::ResourceError::Failed, ["{err:?}"]);
                    }
                },
                Some(b) => {
                    let srcpad = {
                        let state = self.state.lock().unwrap();
                        match &*state {
                            State::Started(started) => started.pad_map.get(&subscribe_id).cloned(),
                            _ => None,
                        }
                    };

                    if let Some(pad) = srcpad {
                        gst::trace!(
                            CAT,
                            imp = self,
                            "Pushing buffer for subscribe id: {subscribe_id}",
                        );

                        if let Err(err) = pad.push(b) {
                            gst::warning!(CAT, imp = self, "Failed to push buffer: {err:?}");
                        }
                    }
                }
            }
        }

        writer.close();

        gst::trace!(CAT, imp = self, "Stream close handler thread exit");
    }
}
