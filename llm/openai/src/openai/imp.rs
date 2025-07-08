use async_tungstenite::tokio::ConnectStream;
use async_tungstenite::WebSocketStream;
use async_tungstenite::{tokio::connect_async, tungstenite::Message};
use futures::stream::{SplitSink, SplitStream};
use futures::SinkExt;
use futures::StreamExt;
use gst::event::CustomDownstream;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use http::Request;
use openai::events::OpenAIEvent;
use parking_lot::Mutex;
use std::sync::{mpsc, LazyLock};
use url::Url;

use crate::openai;
use crate::openai::events::{Content, ConversationItemCreate, ErrorEvent, Item};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "openai",
        gst::DebugColorFlags::empty(),
        Some("OpenAI element"),
    )
});

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("gst-openai-runtime")
        .build()
        .expect("Failed to create tokio runtime")
});

const DEFAULT_LATENCY: gst::ClockTime = gst::ClockTime::from_mseconds(1000);
const DEFAULT_URL: &str = "wss://api.openai.com/v1/realtime";
const DEFAULT_MODEL: &str = "gpt-4o-realtime-preview-2024-12-17";

#[derive(Debug, Clone)]
struct Settings {
    api_key: String,
    model: String,
    latency: gst::ClockTime,
    url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            latency: DEFAULT_LATENCY,
            url: DEFAULT_URL.to_string(),
        }
    }
}

struct State {
    // The receiver is used by the `GstTask` to receive responses from the `tokio` task.
    response_rx: Option<std::sync::mpsc::Receiver<OpenAIEvent>>,
    openai_tx: Option<std::sync::mpsc::Sender<Message>>,
    openai_send_handle: Option<tokio::task::JoinHandle<()>>,
    api_task_handle: Option<tokio::task::JoinHandle<()>>,
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    disconts: Vec<(gst::ClockTime, gst::ClockTime)>,
    pending_discont: bool,
    in_segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    seqnum: gst::Seqnum,
    last_input_rtime: Option<gst::ClockTime>,
    last_response_create_time: Option<gst::ClockTime>,
    connected: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            response_rx: None,
            api_task_handle: None,
            upstream_latency: None,
            disconts: Vec::new(),
            pending_discont: false,
            in_segment: None,
            seqnum: gst::Seqnum::next(),
            last_input_rtime: None,
            last_response_create_time: None,
            connected: false,
            openai_tx: None,
            openai_send_handle: None,
        }
    }
}

enum OpenAIOutput {
    Item(gst::Buffer),
    Event(gst::Event),
}

type WsSink = SplitSink<WebSocketStream<ConnectStream>, Message>;
type WsStream = SplitStream<WebSocketStream<ConnectStream>>;

pub struct OpenAI {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
}

#[glib::object_subclass]
impl ObjectSubclass for OpenAI {
    const NAME: &'static str = "openai";
    type Type = super::OpenAI;
    type ParentType = gst::Element;

    fn with_class(_klass: &Self::Class) -> Self {
        let sink_caps = gst::Caps::builder("text/x-raw")
            .field("format", "utf8")
            .build();
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &sink_caps,
        )
        .expect("Failed to create sink pad template");

        let src_caps = gst::Caps::builder("text/x-raw")
            .field("format", "utf8")
            .build();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &src_caps,
        )
        .expect("Failed to create src pad template");

        let sinkpad = gst::PadBuilder::from_template(&sink_pad_template)
            .chain_function(|pad, parent, buffer| {
                OpenAI::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |openai| openai.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OpenAI::catch_panic_pad_function(
                    parent,
                    || false,
                    |openai| openai.sink_event(pad, event),
                )
            })
            .build();

        let srcpad = gst::PadBuilder::from_template(&src_pad_template)
            .query_function(|pad, parent, query| {
                OpenAI::catch_panic_pad_function(
                    parent,
                    || false,
                    |openai| openai.src_query(pad, query),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            srcpad,
            sinkpad,
        }
    }
}

impl ObjectImpl for OpenAI {
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.sinkpad).expect("Failed to add sink pad");
        obj.add_pad(&self.srcpad).expect("Failed to add src pad");
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("api-key")
                    .nick("API Key")
                    .blurb("OpenAI API Key")
                    .flags(glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING)
                    .build(),
                glib::ParamSpecString::builder("model")
                    .nick("Model")
                    .blurb("OpenAI Chat Model")
                    .default_value(Some(DEFAULT_MODEL))
                    .flags(glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_PLAYING)
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock();
        match pspec.name() {
            "api-key" => {
                settings.api_key = value.get().expect("Type checked by GObject");
            }
            "model" => {
                settings.model = value.get().expect("Type checked by GObject");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock();
        match pspec.name() {
            "api-key" => settings.api_key.to_value(),
            "model" => settings.model.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for OpenAI {}

impl ElementImpl for OpenAI {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "OpenAI",
                "Generic/AI",
                "Sends text to OpenAI's chat API and outputs the response.",
                "Manish <manish@heymanish.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state to {:?}", transition);
        match transition {
            gst::StateChange::PausedToReady => {
                self.disconnect(true);
            }
            _ => (),
        }
        let res = self.parent_change_state(transition)?;
        Ok(res)
    }
}

impl OpenAI {
    fn upstream_latency(&self) -> Option<(bool, gst::ClockTime, Option<gst::ClockTime>)> {
        if let Some(latency) = self.state.lock().upstream_latency {
            return Some(latency);
        }

        let mut peer_query = gst::query::Latency::new();

        let ret = self.sinkpad.peer_query(&mut peer_query);

        if ret {
            let upstream_latency = peer_query.result();
            gst::debug!(
                CAT,
                imp = self,
                "queried upstream latency: {upstream_latency:?}"
            );

            self.state.lock().upstream_latency = Some(upstream_latency);

            Some(upstream_latency)
        } else {
            gst::trace!(CAT, imp = self, "could not query upstream latency");

            None
        }
    }

    fn dequeue(&self, response: OpenAIEvent) -> Vec<OpenAIOutput> {
        let now = self.obj().current_running_time().unwrap();
        let last_rtime = self.state.lock().last_input_rtime;

        let mut output: Vec<OpenAIOutput> = vec![];
        match response {
            OpenAIEvent::ResponseTextDelta(openai::events::ResponseTextDelta { delta, .. }) => {
                gst::debug!(CAT, imp = self, "dequeueing response text delta: {delta:?}");
                let mut buffer = gst::Buffer::with_size(delta.len()).unwrap();
                {
                    let buffer_ref = buffer.get_mut().unwrap();
                    buffer_ref.copy_from_slice(0, delta.as_bytes()).unwrap();
                    buffer_ref.set_flags(gst::BufferFlags::DISCONT);
                    // buffer_ref.set_duration(); // what duration should I set?
                    buffer_ref.set_pts(last_rtime);
                }
                output.push(OpenAIOutput::Item(buffer));
            }
            OpenAIEvent::ResponseTextDone { .. } => {
                gst::debug!(CAT, imp = self, "dequeueing response text done");
                let event = gst::event::CustomDownstream::builder(
                    gst::Structure::builder("openai/response-end")
                        .field("timestamp", now)
                        .build(),
                )
                .build();
                output.push(OpenAIOutput::Event(event));
            }
            _ => {
                gst::warning!(CAT, imp = self, "Unknown event: {response:?}");
            }
        }
        output
    }

    fn start_srcpad_task(&self) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "starting source pad task");

        self.ensure_connection()
            .map_err(|_err| gst::loggable_error!(CAT, "Failed to start pad task: {_err}"))?;

        let this_weak = self.downgrade();
        let res = self.srcpad.start_task(move || {
            loop {
                let Some(this) = this_weak.upgrade() else {
                    break;
                };

                let Some(response_rx) = this.state.lock().response_rx.take() else {
                    gst::debug!(CAT, imp = this, "no more result channel, pausing");
                    let _ = this.srcpad.pause_task();
                    this.disconnect(false);
                    break;
                };

                let Ok(res_evt) = response_rx.recv() else {
                    gst::info!(CAT, imp = this, "no more results, pushing EOS and pausing");
                    let seqnum = this.state.lock().seqnum;
                    let _ = this
                        .srcpad
                        .push_event(gst::event::Eos::builder().seqnum(seqnum).build());
                    let _ = this.srcpad.pause_task();
                    this.disconnect(false);
                    break;
                };

                gst::debug!(CAT, imp = this, "processing result event {res_evt:?}");

                this.state.lock().response_rx = Some(response_rx);

                // NOTE(itzmanish): should we calculate latency or just assume a good enough duration
                // as max latency, because in latency reporting the max latency of the pipeline gets
                // considered.
                if let Some(last_req) = this.state.lock().last_input_rtime {
                    if let Some(curr_running_time) = this.obj().current_running_time() {
                        this.settings.lock().latency =
                            curr_running_time.saturating_sub(last_req.clone());
                    } else {
                        gst::warning!(
                            CAT,
                            imp = this,
                            "Failed to get current running time for latency calculation"
                        );
                    }
                }

                if let OpenAIEvent::Error(ErrorEvent { error, .. }) = res_evt {
                    gst::error!(CAT, imp = this, "error from openai: {error:?}");
                    continue;
                }

                for item in this.dequeue(res_evt).drain(..) {
                    match item {
                        OpenAIOutput::Item(buffer) => {
                            let now = this.obj().current_running_time().unwrap();
                            gst::debug!(CAT, imp = this, "pushing buffer {buffer:?} at {now:?}");

                            if let Err(err) = this.srcpad.push(buffer) {
                                if err != gst::FlowError::Flushing {
                                    gst::element_error!(
                                        this.obj(),
                                        gst::StreamError::Failed,
                                        ["Streaming failed: {}", err]
                                    );
                                }
                                gst::debug!(CAT, imp = this, "pausing task");
                                let _ = this.srcpad.pause_task();
                                this.disconnect(false);
                            }
                        }
                        OpenAIOutput::Event(event) => {
                            gst::debug!(CAT, imp = this, "pushing event {event:?}");

                            this.srcpad.push_event(event);
                        }
                    }
                }
            }
        });

        if res.is_err() {
            return Err(gst::loggable_error!(CAT, "Failed to start pad task"));
        }

        gst::debug!(CAT, imp = self, "started source pad task");

        Ok(())
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::trace!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            StreamStart(_) => {
                gst::info!(CAT, imp = self, "received stream start, connecting");

                if let Err(err) = self.start_srcpad_task() {
                    gst::error!(CAT, imp = self, "Failed to start srcpad task: {err}");
                    return false;
                }

                self.state.lock().seqnum = event.seqnum();

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            FlushStart(_) => {
                gst::info!(CAT, imp = self, "received flush start, disconnecting");
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                let _ = self.state.lock().response_rx.take();
                let _ = self.state.lock().openai_tx.take();
                let _ = self.srcpad.pause_task();
                self.disconnect(false);
                ret
            }
            Segment(e) => {
                {
                    let mut state = self.state.lock();

                    if state.in_segment.is_some() {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Format,
                            ["Multiple segments not supported"]
                        );
                        return false;
                    }

                    let mut segment = match e.segment().clone().downcast::<gst::ClockTime>() {
                        Err(segment) => {
                            gst::element_imp_error!(
                                self,
                                gst::StreamError::Format,
                                ["Only Time segments supported, got {:?}", segment.format(),]
                            );
                            return false;
                        }
                        Ok(segment) => segment,
                    };

                    segment.set_position(segment.start());

                    gst::info!(CAT, imp = self, "using segment {segment:?}");

                    state.in_segment = Some(segment);
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Caps(_) => {
                let caps = gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build();

                let event = gst::event::Caps::builder(&caps)
                    .seqnum(self.state.lock().seqnum)
                    .build();

                self.srcpad.push_event(event);
                true
            }
            Eos(_) => {
                self.state.lock().response_rx.take();
                self.state.lock().openai_tx.take();
                true
            }
            Gap(g) => {
                let (pts, duration) = g.get();

                if let Some(segment) = self.state.lock().in_segment.as_mut() {
                    segment.set_position(match duration {
                        Some(duration) => duration + pts,
                        _ => pts,
                    });
                } else {
                    gst::warning!(CAT, imp = self, "dropping gap before segment");
                    return gst::Pad::event_default(pad, Some(&*self.obj()), event);
                }
                true
            }
            CustomDownstream(c) => {
                gst::debug!(CAT, imp = self, "Handling custom downstream event {c:?}");
                // return self.handle_custom_downstream(c);
                true
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn handle_custom_downstream(&self, event: &CustomDownstream) -> bool {
        let Some(s) = event.structure() else {
            return false;
        };
        match s.name().as_str() {
            "speechtotext/speech-ended" => {
                // Send response create event to trigger OpenAI response
                let response_create = OpenAIEvent::ResponseCreate(openai::events::ResponseCreate {
                    response: openai::events::Response {
                        modalities: vec!["text".to_string()],
                    },
                });
                let response_create_json = serde_json::to_string(&response_create).unwrap();
                gst::debug!(
                    CAT,
                    imp = self,
                    "Sending response create: {}",
                    response_create_json
                );
                let Some(openai_tx) = self.state.lock().openai_tx.take() else {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed sending response create: No ws sink"
                    );
                    return false;
                };
                if let Err(err) = openai_tx.send(Message::Text(response_create_json.into())) {
                    gst::error!(CAT, imp = self, "Failed sending response create: {}", err);
                    self.state.lock().openai_tx = Some(openai_tx);
                    return false;
                }
                self.state.lock().openai_tx = Some(openai_tx);
                true
            }
            _ => true,
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::info!(
            CAT,
            imp = self,
            "Handling {buffer:?}, current running time {}, current clock time {}",
            self.obj().current_running_time().unwrap(),
            self.obj().current_clock_time().unwrap()
        );

        if buffer.pts().is_none() {
            gst::error!(CAT, imp = self, "Only buffers with PTS supported");
            return Err(gst::FlowError::Error);
        }

        if let Err(_err) = self.ensure_connection() {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {_err}"]);
            return Err(gst::FlowError::Error);
        }

        if let Err(_err) = RUNTIME.block_on(self.sync_and_send(buffer)) {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {_err}"]);
            return Err(gst::FlowError::Error);
        }

        Ok(gst::FlowSuccess::Ok)
    }

    async fn sync_and_send(&self, buffer: gst::Buffer) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock();

        if buffer.flags().contains(gst::BufferFlags::DISCONT) {
            gst::info!(
                CAT,
                imp = self,
                "buffer is discont, storing pending discont"
            );
            state.pending_discont = true;
        }

        let Some(segment) = state.in_segment.as_ref() else {
            gst::warning!(CAT, imp = self, "dropping buffer before segment");
            return Ok(());
        };

        let pts = buffer.pts().unwrap();

        let Some(mut rtime) = segment.to_running_time(pts) else {
            gst::log!(CAT, imp = self, "clipping buffer outside segment");
            return Ok(());
        };

        if state.pending_discont {
            state.pending_discont = false;

            let discont_rtime = state.last_input_rtime.unwrap_or(gst::ClockTime::ZERO);

            gst::info!(
                CAT,
                imp = self,
                "storing discont with running time {discont_rtime} and pts {pts}"
            );

            state.disconts.push((discont_rtime, pts));
        }

        rtime.opt_add_assign(buffer.duration());

        gst::trace!(CAT, imp = self, "storing last input running time {rtime}");

        if state.last_input_rtime.is_none() {
            state.in_segment.as_mut().unwrap().set_position(Some(pts));
        }

        state.last_input_rtime = Some(rtime);

        let data = buffer.map_readable().unwrap();
        let text = String::from_utf8(data.to_vec());
        if let Err(e) = text {
            gst::error!(CAT, imp = self, "Failed to convert buffer to text: {}", e);
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to convert buffer to text: {}", e]
            ));
        }
        let text = text.unwrap();

        // Send conversation item create event
        let event = OpenAIEvent::ConversationItemCreate(ConversationItemCreate {
            previous_item_id: None,
            item: Item {
                type_: "message".to_string(),
                role: "user".to_string(),
                content: vec![Content {
                    type_: "input_text".to_string(),
                    text: text.clone(),
                }],
            },
        });
        let event_json = serde_json::to_string(&event).unwrap();
        gst::debug!(CAT, imp = self, "Sending conversation item: {}", event_json);

        if state.openai_tx.is_none() {
            gst::error!(CAT, imp = self, "Failed to send buffer: No ws sink");
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to send buffer: No ws sink"]
            ));
        };

        if let Err(err) = state
            .openai_tx
            .as_ref()
            .unwrap()
            .send(Message::Text(event_json.into()))
        {
            gst::error!(CAT, imp = self, "Failed sending conversation item: {}", err);
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed sending conversation item"]
            ));
        }

        // Send response create event to trigger OpenAI response
        let response_create = OpenAIEvent::ResponseCreate(openai::events::ResponseCreate {
            response: openai::events::Response {
                modalities: vec!["text".to_string()],
            },
        });
        let response_create_json = serde_json::to_string(&response_create).unwrap();
        gst::debug!(
            CAT,
            imp = self,
            "Sending response create: {}",
            response_create_json
        );

        if let Err(err) = state
            .openai_tx
            .as_ref()
            .unwrap()
            .send(Message::Text(response_create_json.into()))
        {
            gst::error!(CAT, imp = self, "Failed sending response create: {}", err);
            return Err(gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed sending response create"]
            ));
        }

        Ok(())
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock();

        if state.connected {
            gst::debug!(CAT, imp = self, "Already connected");
            return Ok(());
        }

        let settings = self.settings.lock();

        gst::info!(CAT, imp = self, "Connecting to openai websocket ..");

        let url = settings.url.clone();

        let mut uri = Url::parse(&url).map_err(|e| {
            gst::error_msg!(
                gst::CoreError::Failed,
                ["Failed to parse provided url: {}", e]
            )
        })?;
        uri.set_query(Some(format!("model={}", settings.model).as_str()));

        let api_key = settings.api_key.clone();
        let authority = uri.authority();
        let host = authority.splitn(2, '@').last().unwrap_or("");

        let request = Request::builder()
            .method("GET")
            .uri(uri.as_str())
            .header("Host", host)
            .header("Upgrade", "websocket")
            .header("Connection", "keep-alive, upgrade")
            .header(
                "Sec-Websocket-Key",
                async_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .header("Sec-Websocket-Version", "13")
            .header("OpenAI-Beta", "realtime=v1")
            .header("Authorization", format!("Bearer {}", &api_key))
            .body(())
            .unwrap();

        let _enter_guard = RUNTIME.enter();

        let (ws, _) = RUNTIME.block_on(connect_async(request)).map_err(|err| {
            gst::error!(CAT, imp = self, "Failed to connect: {}", err);
            gst::error_msg!(gst::CoreError::Failed, ["Failed to connect: {}", err])
        })?;

        drop(_enter_guard);

        gst::debug!(CAT, imp = self, "Connected to openai websocket");

        let _enter_guard = RUNTIME.enter();
        let (ws_sink, mut ws_stream) = RUNTIME.block_on(self.init_connection(ws))?;
        drop(_enter_guard);

        let (openai_tx, openai_rx) = mpsc::channel::<Message>();
        state.openai_tx = Some(openai_tx);

        state.openai_send_handle = Some(RUNTIME.spawn(async move {
            forward_msg_to_ws(ws_sink, openai_rx).await;
        }));

        let (response_tx, response_rx) = mpsc::channel();
        state.response_rx = Some(response_rx);
        state.connected = true;

        let this_weak = self.downgrade();
        let future = async move {
            loop {
                let Some(this) = this_weak.upgrade() else {
                    break;
                };
                let Some(msg) = ws_stream.next().await else {
                    // EOS
                    gst::info!(CAT, imp = this, "Connection closed");
                    break;
                };

                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        gst::error!(CAT, imp = this, "Failed to receive data: {}", err);
                        break;
                    }
                };

                let text = match msg {
                    Message::Text(utf8_bytes) => utf8_bytes.to_string(),
                    Message::Binary(bytes) => String::from_utf8_lossy(&bytes).to_string(),

                    Message::Close(Some(frame)) => {
                        gst::warning!(
                            CAT,
                            imp = this,
                            "OpenAI connection closed: {}",
                            frame.reason
                        );
                        break;
                    }
                    Message::Ping(frame) => {
                        gst::debug!(CAT, imp = this, "Ping received: {frame:?}");
                        this.state
                            .lock()
                            .openai_tx
                            .as_ref()
                            .unwrap()
                            .send(Message::Pong(frame))
                            .unwrap();
                        continue;
                    }
                    _ => {
                        gst::error!(CAT, imp = this, "Unexpected message type: {msg:?}");
                        gst::element_imp_error!(
                            this,
                            gst::StreamError::Failed,
                            ["Unexpected message type: {msg:?}"]
                        );
                        break;
                    }
                };
                let parsed_msg = match serde_json::from_str::<OpenAIEvent>(&text) {
                    Ok(parsed_msg) => parsed_msg,
                    Err(err) => {
                        gst::trace!(CAT, imp = this, "Failed to parse openai event: {err}");
                        continue;
                    }
                };

                if response_tx.send(parsed_msg).is_err() {
                    break;
                }
            }
        };

        state.api_task_handle = Some(RUNTIME.spawn(future));

        gst::info!(CAT, imp = self, "Connected");

        Ok(())
    }

    async fn init_connection(
        &self,
        ws: WebSocketStream<ConnectStream>,
    ) -> Result<(WsSink, WsStream), gst::ErrorMessage> {
        let (mut ws_sink, mut ws_stream) = ws.split();
        let start_message = r#"
        {
            "type": "session.update",
            "session": {
                "modalities": ["text"]
            }
        }
        "#;

        gst::debug!(CAT, imp = self, "Sending start message: {}", start_message);
        ws_sink
            .send(Message::text(start_message))
            .await
            .map_err(|err| {
                gst::error!(CAT, imp = self, "Failed to send StartRecognition: {err}");
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to send StartRecognition: {err}"]
                )
            })?;

        loop {
            let res = ws_stream
                .next()
                .await
                .ok_or_else(|| {
                    gst::error!(CAT, imp = self, "Connection closed unexpectedly");
                    gst::error_msg!(gst::CoreError::Failed, ["Connection closed unexpectedly"])
                })?
                .map_err(|err| {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to receive session.updated event: {err}"
                    );
                    gst::error_msg!(
                        gst::CoreError::Failed,
                        ["Failed to receive session.updated event: {err}"]
                    )
                })?;

            let text = match res {
                Message::Text(text) => Ok(text),
                _ => {
                    gst::error!(CAT, imp = self, "Invalid message type: {res}");
                    Err(gst::error_msg!(
                        gst::CoreError::Failed,
                        ["Invalid message type: {res}"]
                    ))
                }
            }?;

            let parsed_msg: OpenAIEvent = serde_json::from_str(&text).map_err(|err| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to parse session.created event: {err}"
                );
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Failed to parse session.created event: {err}"]
                )
            })?;

            match parsed_msg {
                OpenAIEvent::SessionCreated(session_created) => {
                    gst::debug!(CAT, imp = self, "Session created: {session_created:?}");
                    continue;
                }
                OpenAIEvent::SessionUpdated(session_updated) => {
                    gst::debug!(CAT, imp = self, "Session updated: {session_updated:?}");
                    break;
                }
                OpenAIEvent::Error(error) => {
                    gst::error!(CAT, imp = self, "Error: {error:?}");
                    gst::element_imp_error!(self, gst::StreamError::Failed, ["Error: {error:?}"])
                }
                _ => {
                    gst::error!(CAT, imp = self, "Invalid message type: {text}");
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Failed,
                        ["Invalid message type: {text}"]
                    )
                }
            }
        }

        Ok((ws_sink, ws_stream))
    }

    fn disconnect(&self, stop_task: bool) {
        let mut state = self.state.lock();

        if let Some(handle) = state.api_task_handle.take() {
            gst::info!(CAT, imp = self, "aborting result reception");
            handle.abort();
        }

        if let Some(openai_handle) = state.openai_send_handle.take() {
            gst::info!(CAT, imp = self, "aborting conversation item");
            openai_handle.abort();
        }

        // Make sure the task is fully stopped before resetting the state,
        // in order not to break expectations such as in_segment being
        // present while the task is still processing items
        if stop_task {
            drop(state);
            let _ = self.srcpad.stop_task();
            state = self.state.lock();
        }

        *state = State::default();
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                self.state.lock().upstream_latency = None;

                if let Some(upstream_latency) = self.upstream_latency() {
                    let (live, min, max) = upstream_latency;
                    let our_latency = self.settings.lock().latency;

                    if live {
                        q.set(true, min + our_latency, max.map(|max| max + our_latency));
                    } else {
                        q.set(live, min, max);
                    }
                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

async fn forward_msg_to_ws(mut ws: WsSink, ch: mpsc::Receiver<Message>) {
    while let Ok(msg) = ch.recv() {
        gst::trace!(CAT, "Sending message to ws: {}", msg);
        let _ = ws.send(msg).await.map_err(|err| {
            gst::error!(CAT, "Failed sending response create: {}", err);
            err
        });
    }
    println!("Finished forwarding messages to ws");
}
