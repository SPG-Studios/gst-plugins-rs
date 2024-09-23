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
// QUIC connection or WebTransport session will be shared with `quinnquicsink`
// or `quinnwtsink` downstream. MoQ Control messages require a bi-directional
// stream. This bi-directional stream is the Control channel and all Control
// messages are exchanged on this stream. This bi-directional stream & all
// control messages are handled here.
//
// Actual media is always send on a uni-directional stream by the MoQ relay
// which is handled by either the QUIC or WebTransport element downstream.
//
// For setup the track namespace needs to be published in MoQ speak, media
// will always be send on uni-directional stream by the sink downstream.
// Data send by the sink is muxed into MoQ tracks based on subscribe IDs.
//
// `moqmux` and `moqdemux` are not symmetric. `moqmux` uses the caps sink
// event to assemble the information required by the `.catalog` track. For
// this reason `moqmux`, contains `cmafmux` inside while `moqdemux` does
// not.

/*
 * TODO:
 *
 * - Low Overhead Media Container
 * - Handling all Control messages
 * - Media and event timeline tracks
 * - CMAF switching sets
 * - FETCH support
 * - and probably others
 */

use crate::quinnconnection::*;
use crate::quinnquicmeta::QuinnQuicMeta;
use crate::quinnquicquery::*;
use crate::reader::Reader;
use crate::utils::{
    CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG, Canceller, RUNTIME, WaitError, wait,
};
use crate::writer::Writer;
use crate::{common::*, utils};

use bytes::Bytes;
use gst::{glib, prelude::*, subclass::prelude::*};
use moq_transport::data::{
    ObjectStatus, StreamHeader, StreamHeaderType, SubgroupHeader, SubgroupObject,
};
use moq_transport::{coding::*, message, setup};
use std::collections::HashMap;
use std::{
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    sync::mpsc::{Sender, channel},
    sync::{Arc, LazyLock, Mutex},
};
use tokio::{sync::oneshot, task::JoinHandle};
use url::Url;
use web_transport_quinn::{
    Session,
    proto::{ConnectRequest, ConnectResponse},
};

static CATALOG_TRACK: &str = ".catalog";
static INIT_TRACK_ID: u64 = 0;
static MAX_TRACKS: usize = 16;
static DEFAULT_PUBLISHER_PRIORITY: u8 = 0;
static DEFAULT_FRAGMENT_DURATION: u64 = 2000;
static DEFAULT_CHUNK_DURATION: u64 = u64::MAX;

// Below defaults are for testing with moq-rs.
static DEFAULT_MOQ_TRACK_NAMESPACE: &str = "bbb";
static DEFAULT_MOQ_RELAY_ADDR: &str = "127.0.0.1";
static DEFAULT_MOQ_RELAY_PORT: u16 = 4443;
static DEFAULT_MOQ_SCHEME: &str = "moqt";

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "moqmux",
        gst::DebugColorFlags::empty(),
        Some("Media over QUIC Muxer"),
    )
});

macro_rules! wait_with_errors {
    (
        $canceller:expr,
        $future:expr,
        $timeout:expr,
        $error_msg:expr,
    ) => {{
        match wait($canceller, $future, $timeout) {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(err)) => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["{} failed: {}", $error_msg, err]
            )),
            Err(WaitError::FutureAborted) => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["{} aborted", $error_msg]
            )),
            Err(WaitError::FutureError(err)) => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["{} failed: {}", $error_msg, err]
            )),
        }
    }};
}

#[derive(Default, Clone)]
struct MoqMuxSinkPadSettings {
    track_name: Option<String>,
    priority: Option<u8>,
}

impl From<gst::Structure> for MoqMuxSinkPadSettings {
    fn from(s: gst::Structure) -> Self {
        MoqMuxSinkPadSettings {
            track_name: s.get_optional::<String>("track-name").unwrap(),
            priority: s.get_optional::<u8>("priority").unwrap(),
        }
    }
}

impl From<MoqMuxSinkPadSettings> for gst::Structure {
    fn from(obj: MoqMuxSinkPadSettings) -> Self {
        gst::Structure::builder("track-settings")
            .field_if_some("track-name", obj.track_name)
            .field_if_some("priority", obj.priority)
            .build()
    }
}

#[derive(Default)]
pub(crate) struct MoqMuxSinkPad {
    settings: Mutex<MoqMuxSinkPadSettings>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqMuxSinkPad {
    const NAME: &'static str = "GstMoqMuxSinkPad";
    type Type = super::MoqMuxSinkPad;
    type ParentType = gst::GhostPad;
}

impl ObjectImpl for MoqMuxSinkPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<gst::Structure>("track-settings")
                    .nick("Track Settings")
                    .blurb("MoQ track settings")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "track-settings" => {
                let s = value
                    .get::<gst::Structure>()
                    .expect("Must be a valid structure");
                let mut settings = self.settings.lock().unwrap();
                *settings = s.into();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "track-settings" => gst::Structure::from(settings.clone()).to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for MoqMuxSinkPad {}

impl PadImpl for MoqMuxSinkPad {}

impl ProxyPadImpl for MoqMuxSinkPad {}

impl GhostPadImpl for MoqMuxSinkPad {}

struct Started {
    ctrl_handler: Option<JoinHandle<()>>,
    ctrl_handler_quit: Option<oneshot::Sender<()>>,
    // Track name -> Subscriber ID
    subscriber_ids: HashMap<String, u64>,
    write_task_tx: Option<Sender<WriteRequest>>,
    is_eos: bool,
    // Sink Pad -> Segment
    segment: HashMap<super::MoqMuxSinkPad, gst::FormattedSegment<gst::ClockTime>>,
}

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(channel) = self.write_task_tx.take() {
            let _ = channel.send(WriteRequest::Quit);
        }

        if let Some(channel) = self.ctrl_handler_quit.take() {
            let _ = channel.send(());
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

#[derive(Debug, Clone)]
struct Settings {
    timeout: u32,
    url: Url,
    fragment_duration: u64,
    chunk_duration: u64,
    track_namespace: TrackNamespace,
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

        Self {
            timeout: DEFAULT_TIMEOUT,
            url: relay_url,
            fragment_duration: DEFAULT_FRAGMENT_DURATION,
            chunk_duration: DEFAULT_CHUNK_DURATION,
            track_namespace: TrackNamespace::from_utf8_path(DEFAULT_MOQ_TRACK_NAMESPACE),
        }
    }
}

struct TrackData {
    init_track_buffer: Option<Bytes>,
    segment_track_name: Option<String>,
    init_track_name: Option<String>,
    media_info: Option<MediaInfo>,
    last: Option<(u64, u64)>,
    sink_pad: gst::Pad,
    sequence: AtomicU64,
}

#[derive(Clone)]
struct MediaInfo {
    codec_mime: String,
    codec_data: Option<gst::Buffer>,
    videoinfo: Option<gst_video::VideoInfo>,
    audioinfo: Option<gst_audio::AudioInfo>,
}

impl MediaInfo {
    fn selection_params(&self) -> moq_catalog::SelectionParam {
        let mut selection_param = moq_catalog::SelectionParam {
            codec: Some(self.codec_mime.clone()),
            ..Default::default()
        };

        if let Some(v) = &self.videoinfo {
            selection_param.width = Some(v.width());
            selection_param.height = Some(v.height());
        }

        if let Some(a) = &self.audioinfo {
            selection_param.channel_config = Some(a.channels().to_string());
            selection_param.samplerate = Some(a.rate());
        }

        selection_param
    }

    fn init_data(&self) -> Option<String> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        self.codec_data.as_ref().map(|codec_data| {
            let map = codec_data.map_readable().unwrap();
            STANDARD.encode(map.as_slice())
        })
    }
}

fn get_codec_mime_from_caps(caps: &gst::CapsRef) -> String {
    let mime = gst_pbutils::codec_utils_caps_get_mime_codec(caps);
    mime.map(|s| s.to_string())
        .unwrap_or_else(|_| "application/octet-stream".to_string())
}

impl MediaInfo {
    fn new(caps: &gst::CapsRef) -> Self {
        let codec_mime = get_codec_mime_from_caps(caps);
        let s = caps.structure(0).unwrap();

        let audio = s
            .name()
            .strip_prefix("audio")
            .and_then(|_| gst_audio::AudioInfo::from_caps(caps).ok());

        let video = s
            .name()
            .strip_prefix("video")
            .and_then(|_| gst_video::VideoInfo::from_caps(caps).ok());

        let codec_data = s.get::<gst::Buffer>("codec_data").ok();

        Self {
            codec_mime,
            codec_data,
            audioinfo: audio,
            videoinfo: video,
        }
    }
}

enum WriteRequest {
    Buffer((i32, Vec<u8>, Option<gst::ClockTime>)),
    Event(gst::Event),
    Quit,
}

struct TrackWriter {
    buffer: Vec<Bytes>,
    object_id: u64,
    priority: u8,
    tmp_buffer: Vec<u8>,
    write_task_tx: Sender<WriteRequest>,
    pts: Option<gst::ClockTime>,
}

impl TrackWriter {
    fn new(
        subscribe_id: u64,
        group_id: u64,
        publisher_priority: u8,
        write_task_tx: Sender<WriteRequest>,
        pts: Option<gst::ClockTime>,
    ) -> TrackWriter {
        let mut buffer = Vec::with_capacity(256);

        StreamHeader {
            header_type: StreamHeaderType::SubgroupId,
            subgroup_header: Some(SubgroupHeader {
                header_type: StreamHeaderType::SubgroupId,
                track_alias: subscribe_id,
                group_id,
                subgroup_id: Some(0),
                publisher_priority,
            }),
            fetch_header: None,
        }
        .encode(&mut buffer)
        .expect("Header Subgroup encode should succeed");

        TrackWriter {
            object_id: 0,
            buffer: vec![buffer.into()],
            priority: publisher_priority,
            tmp_buffer: Vec::with_capacity(16),
            write_task_tx,
            pts,
        }
    }

    fn append(&mut self, bytes: Bytes) {
        self.tmp_buffer.clear();

        SubgroupObject {
            object_id_delta: self.object_id,
            payload_length: bytes.len(),
            status: Some(ObjectStatus::NormalObject),
        }
        .encode(&mut self.tmp_buffer)
        .expect("SubgroupObject encode should succeed");

        self.object_id += 1;

        self.buffer.push(self.tmp_buffer.clone().into());
        self.buffer.push(bytes);
    }

    fn write(&mut self) -> Result<(), gst::FlowError> {
        let buffer = {
            let size = self.buffer.iter().map(|b| b.len()).sum();
            let mut vec = Vec::with_capacity(size);
            for bytes in self.buffer.drain(..) {
                vec.extend_from_slice(&bytes);
            }

            vec
        };

        self.write_task_tx
            .send(WriteRequest::Buffer((
                self.priority as i32,
                buffer,
                self.pts,
            )))
            .map_err(|_| gst::FlowError::Error)?;
        self.buffer.clear();

        Ok(())
    }

    fn last_object_id(&self) -> u64 {
        self.object_id
    }
}

pub struct MoqMux {
    srcpad: gst::Pad,
    canceller: Mutex<utils::Canceller>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    sink_pad_counter: AtomicU32,
    // Pad name -> TrackData
    track_data: Mutex<HashMap<super::MoqMuxSinkPad, TrackData>>,
    catalog: Mutex<Option<moq_catalog::Root>>,
}

#[glib::object_subclass]
impl ObjectSubclass for MoqMux {
    const NAME: &'static str = "GstMoqMux";
    type Type = super::MoqMux;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy,);

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ).build();

        Self {
            srcpad,
            canceller: Mutex::new(utils::Canceller::default()),
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            sink_pad_counter: AtomicU32::new(0),
            track_data: Mutex::new(HashMap::new()),
            catalog: Mutex::new(None),
        }
    }
}

impl GstObjectImpl for MoqMux {}

impl BinImpl for MoqMux {}

impl ObjectImpl for MoqMux {
    fn constructed(&self) {
        self.parent_constructed();

        self.obj()
            .add_pad(&self.srcpad)
            .expect("Failed to add src pad");
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
                    .nick("URL")
                    .blurb("MoQ relay URL")
                    .build(),
                glib::ParamSpecString::builder("namespace")
                    .nick("Namespace")
                    .blurb("MoQ namespace for tracks")
                    .build(),
                glib::ParamSpecUInt64::builder("fragment-duration")
                    .nick("Fragment Duration")
                    .blurb("CMAF fragment duration in milliseconds")
                    .default_value(DEFAULT_FRAGMENT_DURATION)
                    .build(),
                glib::ParamSpecUInt64::builder("chunk-duration")
                    .nick("Chunk Duration")
                    .blurb("Duration of each chunk in milliseconds")
                    .default_value(DEFAULT_CHUNK_DURATION)
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
            "fragment-duration" => {
                settings.fragment_duration = value.get::<u64>().unwrap();
            }
            "chunk-duration" => {
                settings.chunk_duration = value.get::<u64>().unwrap();
            }
            "namespace" => {
                settings.track_namespace =
                    TrackNamespace::from_utf8_path(value.get::<String>().unwrap().as_str());
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "timeout" => settings.timeout.to_value(),
            "url" => settings.url.to_string().to_value(),
            "namespace" => settings.track_namespace.to_string().to_value(),
            "fragment-duration" => settings.fragment_duration.to_value(),
            "chunk-duration" => settings.chunk_duration.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl ChildProxyImpl for MoqMux {
    fn children_count(&self) -> u32 {
        let object = self.obj();
        object.num_pads() as u32
    }

    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        let object = self.obj();
        object
            .pads()
            .into_iter()
            .find(|p| p.name() == name)
            .map(|p| p.upcast())
    }

    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        let object = self.obj();
        object
            .pads()
            .into_iter()
            .nth(index as usize)
            .map(|p| p.upcast())
    }
}

impl ElementImpl for MoqMux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Media over QUIC Multiplexer",
                "Source/Network/QUIC",
                "Multiplexes tracks/objects/groups for Media over QUIC",
                "Sanchayan Maity <sanchayan@centricular.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &gst::Caps::new_any(),
                super::MoqMuxSinkPad::static_type(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![sink_pad_template, src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        gst::info!(CAT, imp = self, "Requesting new sink pad");

        let pad_num = self.sink_pad_counter.fetch_add(1, Ordering::SeqCst);
        let pad_name = format!("sink_{}", pad_num);

        let sink_pad = gst::PadBuilder::<super::MoqMuxSinkPad>::from_template(templ)
            .event_function(move |pad, parent, event| {
                MoqMux::catch_panic_pad_function(
                    parent,
                    || false,
                    |mux| mux.sink_event(pad, parent, event),
                )
            })
            .name(&pad_name)
            .build();

        let settings = self.settings.lock().unwrap();
        let cmafmux = gst::ElementFactory::make("cmafmux")
            .name(format!("{}-cmafmux", pad_name))
            .property("fragment-duration", settings.fragment_duration)
            .property("chunk-duration", settings.chunk_duration)
            .property_from_str("header-update-mode", "update")
            .property("write-mehd", true)
            .build()
            .unwrap();
        drop(settings);

        let sink_pad_clone = sink_pad.clone();
        let appsink = gst_app::AppSink::builder()
            .name(format!("{}-appsink", pad_name))
            .buffer_list(true)
            .sync(true)
            .callbacks({
                let self_weak = self.downgrade();

                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |appsink| {
                        let self_ = match self_weak.upgrade() {
                            Some(this) => this,
                            None => return Ok(gst::FlowSuccess::Ok),
                        };

                        let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let buffer_list = sample.buffer_list_owned().expect("no buffer list");

                        let mut track_data = self_.track_data.lock().unwrap();
                        let track = track_data.get_mut(&sink_pad_clone).unwrap();

                        gst::trace!(
                            CAT,
                            imp = self_,
                            "Received buffer list with {} buffers",
                            buffer_list.len()
                        );

                        self_.on_new_sample(buffer_list, track)
                    })
                    .build()
            })
            .build();

        self.obj()
            .add_many([&cmafmux, appsink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([&cmafmux, appsink.upcast_ref()]).unwrap();

        let muxer_sink = cmafmux.static_pad("sink").unwrap();
        sink_pad.set_target(Some(&muxer_sink)).unwrap();

        {
            let track_data = TrackData {
                init_track_buffer: None,
                segment_track_name: None,
                init_track_name: None,
                sink_pad: sink_pad.clone().upcast(),
                sequence: AtomicU64::new(INIT_TRACK_ID),
                media_info: None,
                last: None,
            };

            self.track_data
                .lock()
                .unwrap()
                .insert(sink_pad.clone(), track_data);
        }

        self.obj().add_pad(&sink_pad).unwrap();
        self.obj()
            .child_added(sink_pad.upcast_ref::<gst::Object>(), &sink_pad.name());

        Some(sink_pad.upcast())
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::info!(CAT, imp = self, "change state: {transition:?}");

        match transition {
            gst::StateChange::ReadyToPaused => match self.setup() {
                Ok((session, (reader, writer))) => {
                    let settings = self.settings.lock().unwrap();
                    let namespace = settings.track_namespace.clone();
                    let timeout = settings.timeout;
                    drop(settings);

                    let writer =
                        self.publish_track_namespace(writer, namespace.clone(), timeout)?;

                    let (tx_quit, rx_quit) = oneshot::channel::<()>();
                    let write_task_tx = self.start_write_task()?;

                    let self_ = self.ref_counted();
                    let write_task_tx_clone = write_task_tx.clone();
                    let ctrl_handler = RUNTIME.spawn({
                        let self_ = self_.clone();
                        async move {
                            self_
                                .handle_ctrl_messages(
                                    session,
                                    reader,
                                    writer,
                                    rx_quit,
                                    write_task_tx_clone,
                                    namespace,
                                )
                                .await;
                            gst::debug!(CAT, imp = self_, "Control message handler thread exit");
                        }
                    });

                    let mut state = self.state.lock().unwrap();
                    *state = State::Started(Started {
                        ctrl_handler: Some(ctrl_handler),
                        ctrl_handler_quit: Some(tx_quit),
                        subscriber_ids: HashMap::with_capacity(MAX_TRACKS),
                        write_task_tx: Some(write_task_tx),
                        is_eos: false,
                        segment: HashMap::with_capacity(MAX_TRACKS),
                    });

                    self.populate_track_data();

                    gst::info!(CAT, imp = self, "MoQ setup done");
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "MoQ setup failed: {err:?}");
                    return Err(gst::StateChangeError);
                }
            },
            gst::StateChange::PausedToReady => {
                let mut state = self.state.lock().unwrap();
                if let State::Started(ref mut state) = *state
                    && let Some(channel) = state.write_task_tx.take()
                {
                    let _ = channel.send(WriteRequest::Quit);
                }
            }
            _ => (),
        }

        let ret = self.parent_change_state(transition)?;

        if transition == gst::StateChange::ReadyToNull {
            *self.state.lock().unwrap() = State::Stopped;
            let _ = self.srcpad.stop_task();
            gst::info!(CAT, imp = self, "Stopped");
        }

        Ok(ret)
    }
}

impl MoqMux {
    fn start_write_task(&self) -> Result<Sender<WriteRequest>, gst::StateChangeError> {
        let (write_task_tx, read_task_rx) = channel::<WriteRequest>();
        let srcpad = self.srcpad.clone();

        let loop_fn = move || {
            loop {
                match read_task_rx.recv() {
                    Ok(write_req) => match write_req {
                        WriteRequest::Buffer((priority, buffer, pts)) => {
                            // Requesting a stream is a blocking call. See the open_stream
                            // implementation in sink. Also, if the remote end reaches the
                            // upper limit on open streams, stream request downstream will
                            // block.
                            if let Some(stream_id) = request_stream(&srcpad, priority) {
                                let mut buffer = gst::Buffer::from_slice(buffer);
                                {
                                    let buf = buffer.get_mut().unwrap();
                                    buf.set_pts(pts);
                                    QuinnQuicMeta::add(buf, stream_id, false);
                                }

                                gst::trace!(CAT, "Pushing {buffer:?} with stream_id: {stream_id}");

                                match srcpad.push(buffer) {
                                    Ok(_) => {
                                        close_stream(&srcpad, stream_id);
                                    }
                                    Err(err) => {
                                        if err == gst::FlowError::Eos {
                                            gst::debug!(CAT, "Not pushing buffer, got Eos");
                                            break;
                                        } else {
                                            gst::warning!(CAT, "Failed to push buffer: {err:?}");
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        WriteRequest::Event(event) => {
                            let _ = srcpad.push_event(event);
                        }
                        WriteRequest::Quit => break,
                    },
                    Err(err) => {
                        gst::debug!(CAT, "Write channel closed: {err:?}");
                        break;
                    }
                }
            }

            let _ = srcpad.pause_task();

            gst::debug!(CAT, "Exiting srcpad task");
        };

        self.srcpad.start_task(loop_fn).map_err(|_| {
            gst::error!(CAT, "Failed to start srcpad task");
            gst::StateChangeError
        })?;

        Ok(write_task_tx)
    }

    // Request the QUIC connection or WebTransport session from downstream
    fn setup_shared_session(&self) -> Result<QuinnConnection, gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Requesting Quinn Connection Context");

        utils::setup_shared_session(self.obj().clone().into(), &self.srcpad)
    }

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

    fn setup_moq_session(&self, session: &Session) -> Result<(Reader, Writer), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let timeout = settings.timeout;
        drop(settings);

        wait_with_errors!(
            &self.canceller,
            self.send_setup(session.clone()),
            timeout,
            "Session setup",
        )
    }

    fn publish_catalog(
        &self,
        subscriber_id: u64,
        write_task_tx: Sender<WriteRequest>,
    ) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Publishing catalog");

        let catalog = self.catalog.lock().unwrap();
        let catalog = catalog.as_ref().expect("Catalog should be available");

        let catalog_str =
            serde_json::to_string_pretty(&catalog).expect("Catalog to JSON must succeed");

        let mut track_writer = TrackWriter::new(
            subscriber_id,
            0,
            DEFAULT_PUBLISHER_PRIORITY,
            write_task_tx,
            Some(gst::ClockTime::ZERO),
        );
        track_writer.append(catalog_str.into());

        track_writer
            .write()
            .map(|_| {
                gst::debug!(CAT, imp = self, "Published catalog");
            })
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to write Catalog {err:?}"]
                )
            })
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
                            ["Failed to send server setup message"]
                        ));
                    }
                    Err(err) => {
                        return Err(gst::error_msg!(
                            gst::ResourceError::Failed,
                            ["Failed to send server setup message: {err:?}"]
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

    fn setup(&self) -> Result<(Arc<Session>, (Reader, Writer)), gst::ErrorMessage> {
        let shared_session = self.setup_shared_session()?;
        let session = self.open_session(shared_session);
        let (reader, writer) = self.setup_moq_session(&session)?;

        Ok((session, (reader, writer)))
    }

    fn publish_track_namespace(
        &self,
        mut writer: Writer,
        track_namespace: TrackNamespace,
        timeout: u32,
    ) -> Result<Writer, gst::StateChangeError> {
        gst::info!(
            CAT,
            imp = self,
            "Publishing track namespace: {track_namespace}"
        );

        let publish_namespace: message::Message = message::PublishNamespace {
            // See Section 9.1. We only ever initiate the connection and
            // won't support multiple streams from a single publisher.
            id: 0,
            track_namespace,
            params: Default::default(),
        }
        .into();

        wait_with_errors!(
            &self.canceller,
            writer.encode(&publish_namespace),
            timeout,
            "Publish track namespace",
        )
        .map_err(|_| gst::StateChangeError)?;

        Ok(writer)
    }

    fn populate_track_data(&self) {
        let mut track_data = self.track_data.lock().unwrap();

        for (pad, track_data) in track_data.iter_mut() {
            let track_name = pad
                .imp()
                .settings
                .lock()
                .unwrap()
                .track_name
                .clone()
                .unwrap_or_else(|| pad.name().to_string());
            // TODO: Make these fully configurable for the user later?.
            let init_track_name = format!("{}_init.mp4", track_name);
            let segment_track_name = format!("{}.m4s", track_name);

            track_data.init_track_name = Some(init_track_name);
            track_data.segment_track_name = Some(segment_track_name);
        }
    }

    fn create_catalog(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let namespace = settings.track_namespace.clone();
        drop(settings);

        let mut tracks = Vec::new();
        let mut track_data = self.track_data.lock().unwrap();

        for (pad, track) in track_data.iter_mut() {
            let Some(media_info) = track.media_info.as_ref() else {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Missing media info for track on {pad:?}"]
                ));
            };

            let catalog_track = moq_catalog::Track {
                init_track: track.init_track_name.clone(),
                name: track.segment_track_name.clone().unwrap(),
                namespace: Some(namespace.clone().to_string()),
                packaging: Some(moq_catalog::TrackPackaging::Cmaf),
                render_group: Some(1),
                selection_params: media_info.selection_params(),
                init_data: media_info.init_data(),
                ..Default::default()
            };

            tracks.push(catalog_track);
        }

        let catalog = moq_catalog::Root {
            version: 1,
            streaming_format: 1,
            streaming_format_version: "0.2".to_string(),
            streaming_delta_updates: false,
            common_track_fields: moq_catalog::CommonTrackFields::from_tracks(&mut tracks),
            tracks,
        };

        gst::info!(CAT, imp = self, "Created catalog {catalog:?}");

        *self.catalog.lock().unwrap() = Some(catalog);

        Ok(())
    }

    fn on_new_sample(
        &self,
        mut buffer_list: gst::BufferList,
        track: &mut TrackData,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let moqmux_sinkpad = track
            .sink_pad
            .clone()
            .downcast::<super::MoqMuxSinkPad>()
            .unwrap();
        let priority = moqmux_sinkpad
            .imp()
            .settings
            .lock()
            .unwrap()
            .priority
            .unwrap_or(DEFAULT_PUBLISHER_PRIORITY);

        let (track_subscriber_id, write_task_tx, first_pts) = {
            let mut state = self.state.lock().unwrap();
            let State::Started(ref mut state) = *state else {
                gst::error!(CAT, imp = self, "Not in started state");
                return Err(gst::FlowError::Error);
            };

            if state.is_eos {
                return Err(gst::FlowError::Eos);
            }

            let write_task_tx = state.write_task_tx.as_ref().unwrap().clone();

            assert!(!buffer_list.is_empty());

            let segment_track_name = track.segment_track_name.as_ref().unwrap().clone();
            let init_track_name = track.init_track_name.as_ref().unwrap().clone();

            let segment = state.segment.get(&moqmux_sinkpad).unwrap();
            let first = buffer_list.get(0).unwrap();
            let first_pts = segment.to_running_time(first.pts().unwrap());

            if first
                .flags()
                .contains(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER)
            {
                let map = first.map_readable().unwrap();
                track.init_track_buffer = Some(Bytes::from_owner(map.to_vec()));
                drop(map);

                buffer_list.make_mut().remove(0..1);
            }

            // Till we have a subscriber, there is no way for us to send the buffers.
            if let Some(init_subscriber_id) = state.subscriber_ids.get(&init_track_name)
                && let Some(buffer) = track.init_track_buffer.take()
            {
                self.write_init_track(
                    &track.sink_pad,
                    buffer,
                    *init_subscriber_id,
                    priority,
                    write_task_tx.clone(),
                    first_pts,
                )?;
            };

            if buffer_list.is_empty() {
                return Ok(gst::FlowSuccess::Ok);
            }

            let Some(track_subscriber_id) = state.subscriber_ids.get(&segment_track_name) else {
                gst::trace!(
                    CAT,
                    imp = self,
                    "No subscriber id for pad {:?}",
                    track.sink_pad
                );
                return Ok(gst::FlowSuccess::Ok);
            };
            let track_subscriber_id = *track_subscriber_id;

            (track_subscriber_id, write_task_tx, first_pts)
        };

        let segment_sequence = track.sequence.fetch_add(1, Ordering::SeqCst);
        let mut track_writer = TrackWriter::new(
            track_subscriber_id,
            segment_sequence,
            priority,
            write_task_tx,
            first_pts,
        );

        for buffer in buffer_list.iter() {
            let map = buffer.map_readable().unwrap();
            track_writer.append(Bytes::from_owner(map.to_vec()));
        }

        track_writer.write()?;

        let last_object_id = track_writer.last_object_id();

        gst::trace!(
            CAT,
            imp = self,
            "Published group {} object {} for pad {:?}",
            segment_sequence,
            last_object_id,
            track.sink_pad
        );

        track.last = Some((segment_sequence, last_object_id));

        Ok(gst::FlowSuccess::Ok)
    }

    fn write_init_track(
        &self,
        track_pad: &gst::Pad,
        bytes: Bytes,
        init_subscriber_id: u64,
        priority: u8,
        write_task_tx: Sender<WriteRequest>,
        pts: Option<gst::ClockTime>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut init_track_writer = TrackWriter::new(
            init_subscriber_id,
            INIT_TRACK_ID,
            priority,
            write_task_tx,
            pts,
        );

        init_track_writer.append(bytes);
        init_track_writer.write()?;

        gst::debug!(
            CAT,
            imp = self,
            "Wrote init segment for track {track_pad:?}",
        );

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(
        &self,
        pad: &super::MoqMuxSinkPad,
        parent: Option<&impl IsA<gst::Object>>,
        mut event: gst::Event,
    ) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(caps) => {
                let caps = caps.caps();

                gst::debug!(
                    CAT,
                    imp = self,
                    "Received caps event on pad {}: {:?}",
                    pad.name(),
                    caps
                );

                let have_all_media_info = {
                    let mut track_data = self.track_data.lock().unwrap();

                    let Some(track) = track_data.get_mut(pad) else {
                        gst::warning!(CAT, imp = self, "No track found for pad {}", pad.name());
                        return false;
                    };

                    track.media_info = Some(MediaInfo::new(caps));
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Updated media info for track {}",
                        pad.name()
                    );

                    track_data.values().all(|t| t.media_info.is_some())
                };

                if have_all_media_info {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "All tracks have media info, creating catalog"
                    );

                    if let Err(e) = self.create_catalog() {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Failed,
                            ["Failed to create Catalog: {e:?}"]
                        );
                    }
                }
            }
            EventView::Eos(_) => {
                gst::debug!(CAT, imp = self, "Received Eos event on pad {}", pad.name());

                let mut state = self.state.lock().unwrap();
                match &mut *state {
                    State::Started(state) => {
                        state.is_eos = true;
                        if let Some(tx) = &state.write_task_tx {
                            let _ = tx.send(WriteRequest::Event(gst::event::Eos::new()));
                        }
                    }
                    _ => {
                        drop(state);
                        self.srcpad.push_event(gst::event::Eos::new());
                    }
                }
            }
            EventView::FlushStart(_) => {
                let mut canceller = self.canceller.lock().unwrap();
                canceller.abort();
            }
            EventView::FlushStop(_) => {
                let mut canceller = self.canceller.lock().unwrap();
                *canceller = Canceller::None;
            }
            EventView::Segment(ev) => {
                let mut state = self.state.lock().unwrap();
                if let State::Started(started) = &mut *state {
                    let segment = if ev.segment().format() != gst::Format::Time {
                        gst::warning!(
                            CAT,
                            obj = pad,
                            "Received non-TIME segment, replacing with default TIME segment"
                        );

                        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                        event = gst::event::Segment::builder(&segment)
                            .seqnum(event.seqnum())
                            .build();
                        segment
                    } else {
                        ev.segment().clone().downcast::<gst::ClockTime>().unwrap()
                    };

                    started.segment.insert(pad.clone(), segment);
                }
            }
            _ => (),
        }

        gst::Pad::event_default(pad, parent, event)
    }

    async fn handle_ctrl_messages(
        &self,
        session: Arc<Session>,
        mut reader: Reader,
        mut writer: Writer,
        mut receiver: oneshot::Receiver<()>,
        write_task_tx: Sender<WriteRequest>,
        track_namespace: TrackNamespace,
    ) {
        const MAX_CONSECUTIVE_ERRORS: u32 = 8;
        let mut consecutive_errors = 0;
        let mut subscriber_ids: HashMap<String, u64> = HashMap::new();

        gst::debug!(CAT, imp = self, "Starting control message handler thread");

        loop {
            let (res, quit): (Option<message::Message>, bool) = tokio::select! {
                    biased;
                    quit = &mut receiver => match quit {
                        Ok(_) => (None, true),
                        Err(e) => {
                            gst::error!(CAT, imp = self, "Error in oneshot channel {e:?}");
                            (None, true)
                        },
                    },
                    res = reader.decode::<message::Message>() => {
                        match res {
                            Ok(s) => (s, false),
                            Err(e) => {
                                consecutive_errors += 1;

                                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                    gst::error!(CAT, imp = self, "Failed to decode message {e:?} ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS})");
                                    (None, true)
                                } else {
                                    gst::warning!(CAT, imp = self, "Failed to decode message {e:?}");
                                    (None, false)
                                }
                            },
                        }
                    },
            };

            if quit {
                break;
            }

            if let Some(s) = res {
                match s {
                    message::Message::Subscribe(s) => {
                        gst::debug!(CAT, imp = self, "{s:?}");

                        let subscriber_id = s.id;
                        let track_name = s.track_name.clone();

                        if track_name == CATALOG_TRACK {
                            // Catalog track is always send first.
                            self.send_stream_start_and_segment_event(write_task_tx.clone());

                            // See `Catalog` section of MoQ Streaming Format.
                            if let Err(e) =
                                self.publish_catalog(subscriber_id, write_task_tx.clone())
                            {
                                gst::element_imp_error!(
                                    self,
                                    gst::ResourceError::Failed,
                                    ["Failed to publish Catalog: {e:?}"]
                                );
                            }
                        }

                        {
                            let mut state = self.state.lock().unwrap();
                            let State::Started(ref mut state) = *state else {
                                continue;
                            };

                            state
                                .subscriber_ids
                                .insert(track_name.clone(), subscriber_id);
                            subscriber_ids.insert(track_name, subscriber_id);
                        }

                        let s_ok: message::Message = message::SubscribeOk {
                            id: subscriber_id,
                            track_alias: subscriber_id,
                            expires: 0,
                            group_order: message::GroupOrder::Ascending,
                            content_exists: false,
                            largest_location: None,
                            params: Default::default(),
                        }
                        .into();

                        let _ = writer.encode(&s_ok).await;
                    }
                    message::Message::Unsubscribe(u) => {
                        gst::debug!(CAT, imp = self, "Unsubscribe {u:?}");

                        let unsubscribe_id = u.id;

                        let mut state = self.state.lock().unwrap();
                        if let State::Started(ref mut state) = *state {
                            state.subscriber_ids.retain(|_, v| *v != unsubscribe_id);
                            subscriber_ids.retain(|_, v| *v != unsubscribe_id);
                        }
                    }
                    message::Message::PublishNamespaceOk(a) => {
                        gst::info!(CAT, imp = self, "PublishNamespaceOk {a:?}");
                    }
                    message::Message::PublishNamespaceError(a) => {
                        gst::warning!(CAT, imp = self, "PublishNamespaceError {a:?}");
                    }
                    message::Message::PublishNamespaceCancel(c) => {
                        gst::info!(CAT, imp = self, "PublishNamespaceCancel {c:?}");
                    }
                    m => {
                        gst::warning!(CAT, imp = self, "Unhandled control message: {m:?}");
                    }
                }
            }
        }

        for s_id in subscriber_ids.values() {
            let s_ok: message::Message = message::PublishDone {
                id: *s_id,
                status_code: CONNECTION_CLOSE_CODE as u64,
                reason: ReasonPhrase(CONNECTION_CLOSE_MSG.to_string()),
                stream_count: 0, // TODO: Update with the actual number of streams requested
            }
            .into();

            let _ = writer.encode(&s_ok).await;

            gst::debug!(CAT, imp = self, "SubscribeDone for subscriber_id: {s_id}");
        }

        gst::debug!(
            CAT,
            imp = self,
            "Publish track namespace done: {track_namespace}"
        );

        let namespace_done: message::Message =
            message::PublishNamespaceDone { track_namespace }.into();

        let _ = writer.encode(&namespace_done).await;

        gst::debug!(CAT, imp = self, "Closing MoQ session");

        writer.close();

        session.close(CONNECTION_CLOSE_CODE, CONNECTION_CLOSE_MSG.as_bytes());
    }

    fn send_stream_start_and_segment_event(&self, write_request_tx: Sender<WriteRequest>) {
        let stream_start_evt = gst::event::StreamStart::builder("catalog")
            .group_id(gst::GroupId::next())
            .build();
        let _ = write_request_tx.send(WriteRequest::Event(stream_start_evt));

        let segment_evt = gst::event::Segment::new(&gst::FormattedSegment::<gst::ClockTime>::new());
        let _ = write_request_tx.send(WriteRequest::Event(segment_evt));
    }
}
