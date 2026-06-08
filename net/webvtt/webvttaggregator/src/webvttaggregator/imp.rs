use gst::subclass::ElementMetadata;
use gst::{glib, prelude::*, subclass::prelude::*, Buffer, Caps, Clock, ClockTime, Event, EventView, FlowError, FlowSuccess};
use gst_base::prelude::{AggregatorExt, AggregatorExtManual, AggregatorPadExt};
use gst_base::subclass::prelude::{AggregatorImpl, AggregatorPadImpl};
use gst_base::AggregatorPad;
use std::str::FromStr;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use gst::FlowError::Eos;
use vtt::prelude::*;

const DEFAULT_WEB_VTT_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(1);

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webvttaggregator",
        gst::DebugColorFlags::empty(),
        Some("webvtt aggregator"),
    )
});

///////////////////////////////////////////////////////////////////////////////
// WebVTTAggregatorPad

#[derive(Default)]
pub(crate) struct WebVTTAggregatorPad {}

#[glib::object_subclass]
impl ObjectSubclass for WebVTTAggregatorPad {
    const NAME: &'static str = "WebVTTAggregatorPad";
    type Type = super::WebVTTAggregatorPad;
    type ParentType = gst_base::AggregatorPad;
}

impl ObjectImpl for WebVTTAggregatorPad {}

impl GstObjectImpl for WebVTTAggregatorPad {}

impl PadImpl for WebVTTAggregatorPad {}

impl AggregatorPadImpl for WebVTTAggregatorPad {}

///////////////////////////////////////////////////////////////////////////////

pub struct VttCueBuffer {
    pst: ClockTime,
    duration: ClockTime,
    vtt_cue: VttCue
}

impl VttCueBuffer {
    fn new(pst: ClockTime, duration: ClockTime, vtt_cue: VttCue) -> Self {
        Self {
            pst,
            duration,
            vtt_cue,
        }
    }
}

pub struct WebVttState {
    pub web_vtt: WebVtt,
    pub completed: bool,
}

impl WebVttState {
    fn new(web_vtt: WebVtt, completed: bool,) -> Self {
        Self {
            web_vtt,
            completed,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Settings {
    target_duration: gst::ClockTime,
}

#[derive(Default)]
pub(crate) struct WebVTTAggregatorState {
    web_vtt_chunk: WebVtt,
    current_web_vtt_chunk_start_time: gst::ClockTime,
    current_web_vtt_chunk_end_time: gst::ClockTime,
    next_aggregation_chunk_time: Option<gst::ClockTime>,
}

#[derive(Default)]
pub struct WebVTTAggregator {
    settings: Mutex<Settings>,
    state: Mutex<WebVTTAggregatorState>,
}

impl WebVTTAggregator {
    fn move_to_next_web_vtt_chunk_from(&self, start_time: gst::ClockTime) {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        state.current_web_vtt_chunk_start_time = start_time;
        state.current_web_vtt_chunk_end_time = start_time + settings.target_duration;
        state.next_aggregation_chunk_time = Some(state.current_web_vtt_chunk_end_time);
    }

    fn split_web_vtt_cue(
        &self,
        current_web_vtt_start_time: ClockTime,
        current_web_vtt_end_time: ClockTime,
        current_web_vtt_cue: VttCue,
    ) -> (VttCue, VttCue) {
        let state = self.state.lock().unwrap();

        // Redefining the current web vtt end time to be the end time of the current chunk
        let mut first_web_vtt_cue = current_web_vtt_cue.clone();

        let new_current_web_vtt_start_time = current_web_vtt_start_time;
        let new_current_web_vtt_end_time = state.current_web_vtt_chunk_end_time;

        first_web_vtt_cue.start = VttTimestamp::new(Duration::from_nanos(
            new_current_web_vtt_start_time.nseconds(),
        ));
        first_web_vtt_cue.end = VttTimestamp::new(Duration::from_nanos(
            new_current_web_vtt_end_time.nseconds(),
        ));

        // Creating next web vtt start time to be the end time of the current chunk
        let mut second_web_vtt_cue = current_web_vtt_cue.clone();

        let next_web_vtt_start_time = state.current_web_vtt_chunk_end_time;
        let next_web_vtt_end_time = current_web_vtt_end_time;

        second_web_vtt_cue.start =
            VttTimestamp::new(Duration::from_nanos(next_web_vtt_start_time.nseconds()));
        second_web_vtt_cue.end =
            VttTimestamp::new(Duration::from_nanos(next_web_vtt_end_time.nseconds()));

        (first_web_vtt_cue, second_web_vtt_cue)
    }

    fn serialize_web_vtt_chunk(&self, chunk: &WebVttState) -> Result<Buffer, gst::FlowError> {
        let state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let serialized_web_vtt = chunk.web_vtt.to_string();

        gst::info!(CAT, imp = self, "VTT: {:?}, current_web_vtt_chunk_start_time: {:?}, target_duration: {:?}", serialized_web_vtt, state.current_web_vtt_chunk_start_time, settings.target_duration);

        let mut web_vtt_output_buffer = gst::Buffer::from_slice(serialized_web_vtt);
        let web_vtt_output_buffer_mutable =
            web_vtt_output_buffer.get_mut().ok_or(FlowError::Error)?;

        web_vtt_output_buffer_mutable.set_pts(state.current_web_vtt_chunk_start_time);
        web_vtt_output_buffer_mutable.set_duration(settings.target_duration);

        Ok(web_vtt_output_buffer)
    }

    fn add_web_vtt_cue_to_chunk(&self, web_vtt_cue: VttCue) {
        let mut state = self.state.lock().unwrap();

        state.web_vtt_chunk.add_cue(web_vtt_cue);
    }

    fn add_web_vtt_cues_to_chunk(&self, web_vtt_cues: Vec<VttCue>) {
        let mut state = self.state.lock().unwrap();

        state.web_vtt_chunk.cues.extend(web_vtt_cues);
    }

    fn move_to_next_web_vtt_chunk(&self) {
        let state = self.state.lock().unwrap();
        let current_web_vtt_chunk_end_time = state.current_web_vtt_chunk_end_time;

        drop(state);

        self.move_to_next_web_vtt_chunk_from(current_web_vtt_chunk_end_time);
    }

    fn move_to_first_web_vtt_chunk(&self, start_time: gst::ClockTime) {
        let state = self.state.lock().unwrap();
        let next_aggregation_chunk_time = state.next_aggregation_chunk_time;

        drop(state);

        if next_aggregation_chunk_time.is_none() {
            // First cue sets the first chunk start time
            self.move_to_next_web_vtt_chunk_from(start_time);
        }
    }

    fn drain_sink(&self, sink: super::WebVTTAggregatorPad) -> Result<Vec<VttCueBuffer>, FlowError> {
        let mut cue_buffers_to_process: Vec<VttCueBuffer> = Vec::new();

        // Drain all the buffers available at the moment
        while let Some(current_web_vtt_buffer) = sink.pop_buffer() {
            let buffer_mapped = current_web_vtt_buffer.map_readable().map_err(|e| {
                gst::error!(CAT, imp = self, "Error mapping output buffer: {e}");

                FlowError::Error
            })?;

            let web_vtt_string = std::str::from_utf8(buffer_mapped.as_slice()).map_err(|e| {
                gst::error!(CAT, imp = self, "Error mapping to utf8: {e}");

                FlowError::Error
            })?;

            gst::info!(CAT, imp = self, "Cue: {:?}", web_vtt_string);

            if web_vtt_string == "WEBVTT\n\n" {
                // Initial header, we consume it and move on to next cue arriving
                continue;
            }

            let current_cue_start_time = current_web_vtt_buffer.pts().ok_or_else(|| {
                gst::error!(CAT, imp = self, "input buffers must have PTS, got None");

                FlowError::Error
            })?;

            let current_cue_duration = current_web_vtt_buffer.duration().ok_or_else(|| {
                gst::error!(
                    CAT,
                    imp = self,
                    "input buffers must have Duration, got None"
                );

                FlowError::Error
            })?;

            let current_cue = VttCue::from_str(web_vtt_string).map_err(|e| {
                gst::error!(CAT, imp = self, "Error creating WebVtt object: {e}");

                FlowError::Error
            })?;

            self.move_to_first_web_vtt_chunk(current_cue_start_time);

            let vtt_cue_buffer = VttCueBuffer::new(current_cue_start_time, current_cue_duration, current_cue);

            cue_buffers_to_process.push(vtt_cue_buffer);
        }

        Ok(cue_buffers_to_process)
    }

    fn build_web_vtt_chunks(&self, timeout: bool, cue_buffers_to_process: Vec<VttCueBuffer>) -> Vec<WebVttState> {
        let state = self.state.lock().unwrap();

        let current_web_vtt_chunk_end_time = state.current_web_vtt_chunk_end_time;
        let next_aggregation_chunk_time = state.next_aggregation_chunk_time;
        let uncompleted_web_vtt_chunk = state.web_vtt_chunk.clone();

        drop(state);

        let web_vtt_state = WebVttState::new(uncompleted_web_vtt_chunk, false);
        let mut web_vtt_chunks: Vec<WebVttState> = vec![web_vtt_state];

        for cue_buffer_to_process in cue_buffers_to_process {
            let uncompleted_web_vtt_chunk: &mut WebVttState = web_vtt_chunks.last_mut().unwrap();

            let current_cue_start_time = cue_buffer_to_process.pst;
            let current_cue_duration = cue_buffer_to_process.duration;
            let current_cue = cue_buffer_to_process.vtt_cue;

            let current_cue_end_time = current_cue_start_time + current_cue_duration;

            if current_cue_start_time.nseconds() < current_web_vtt_chunk_end_time.nseconds() {
                if current_cue_end_time.nseconds() <= current_web_vtt_chunk_end_time.nseconds() {
                    // Cue is fully inside the current webvtt chunk
                    uncompleted_web_vtt_chunk.web_vtt.add_cue(current_cue);
                } else {
                    // Cue overlaps with the next target duration
                    let (current_new_cue, next_new_cue) = self.split_web_vtt_cue(
                        current_cue_start_time,
                        current_cue_end_time,
                        current_cue,
                    );

                    // Adding the first partial cue to the current chunk
                    uncompleted_web_vtt_chunk.web_vtt.add_cue(current_new_cue);
                    uncompleted_web_vtt_chunk.completed = true;

                    // Adding the second partial cue to next chunk
                    let mut new_uncompleted_web_vtt_chunk = WebVttState::new(WebVtt::new(), false);
                    new_uncompleted_web_vtt_chunk.web_vtt.add_cue(next_new_cue);
                    web_vtt_chunks.push(new_uncompleted_web_vtt_chunk);
                }
            } else {
                //We assume previous chunk is completed
                uncompleted_web_vtt_chunk.completed = true;

                // Current web vtt start time doesn't belong to the current chuck
                let mut new_uncompleted_web_vtt_chunk = WebVttState::new(WebVtt::new(), false);
                new_uncompleted_web_vtt_chunk.web_vtt.add_cue(current_cue);
                web_vtt_chunks.push(new_uncompleted_web_vtt_chunk);
            };
        }

        web_vtt_chunks
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WebVTTAggregator {
    const NAME: &'static str = "webvttaggregator";
    type Type = super::WebVTTAggregator;
    type ParentType = gst_base::Aggregator;
}

impl ObjectImpl for WebVTTAggregator {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![glib::ParamSpecUInt64::builder("target-duration")
                .nick("Target duration")
                .blurb("The target duration in seconds of a  webvtt file")
                .default_value(DEFAULT_WEB_VTT_DURATION.nseconds())
                .mutable_ready()
                .build()]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        gst::debug!(
            CAT,
            imp = self,
            "Setting property '{}' to '{:?}'",
            pspec.name(),
            value
        );

        match pspec.name() {
            "target-duration" => {
                settings.target_duration = value.get().expect("type checked upstream");

                self.obj().set_latency(settings.target_duration, None);
            }

            _ => unimplemented!(),
        };
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "target-duration" => settings.target_duration.to_value(),

            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        let class = obj.class();
        let pad_template = class.pad_template("sink").unwrap();

        let sinkpad = gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&pad_template)
            .flags(gst::PadFlags::ACCEPT_INTERSECT)
            .build();

        obj.add_pad(&sinkpad).unwrap();
    }
}

impl GstObjectImpl for WebVTTAggregator {}

impl ElementImpl for WebVTTAggregator {
    fn metadata() -> Option<&'static ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            ElementMetadata::new(
                "WebVTT aggregator",
                "Aggregator",
                "WebVTT aggregator",
                "genius sports",
            )
        });

        Some(&*ELEMENT_METADATA)
    }
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = Caps::builder("application/x-subtitle-vtt").build();
            let src_caps = Caps::builder("application/x-subtitle-vtt").build();

            vec![
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &src_caps,
                )
                    .unwrap(),
                gst::PadTemplate::with_gtype(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &sink_caps,
                    super::WebVTTAggregatorPad::static_type(),
                )
                    .unwrap(),
            ]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for WebVTTAggregator {

    fn aggregate(&self, timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();

        let current_web_vtt_chunk_end_time = state.current_web_vtt_chunk_end_time;
        let next_aggregation_chunk_time = state.next_aggregation_chunk_time;

        drop(state);

        gst::info!(
                    CAT,
                    imp = self,
                    "Aggregate: timeout={:?}, current_web_vtt_chunk_end_time: {:?}, next_aggregation_chunk_time: {:?}",
                    timeout,
                    current_web_vtt_chunk_end_time,
                    next_aggregation_chunk_time
                );

        let sink = self
            .obj()
            .sink_pads()
            .into_iter()
            .map(|pad| pad.downcast::<super::WebVTTAggregatorPad>().unwrap())
            .next()
            .ok_or_else(|| {
                gst::error!(CAT, imp = self, "Error getting sink");

                FlowError::Error
            })?;

        let is_eos = sink.is_eos();

        let cue_buffers_to_process = self.drain_sink(sink)?;

        if is_eos & cue_buffers_to_process.is_empty() {
            return Err(Eos);
        }

        // target duration is done, we publish whatever we have
        if timeout & cue_buffers_to_process.is_empty() {
            gst::info!(
                    CAT,
                    imp = self,
                    "Timeout and no cue_buffers_to_process",
                );

            let mut state = self.state.lock().unwrap();

            let web_vtt_chunk = state.web_vtt_chunk.clone();

            state.web_vtt_chunk.cues.clear();

            drop(state);

            let web_vtt_output_buffer = self.serialize_web_vtt_chunk(&WebVttState::new(web_vtt_chunk, false))?;

            self.move_to_next_web_vtt_chunk();

            // Sending the webvtt chunk
            return self.finish_buffer(web_vtt_output_buffer);
        }

        let web_vtt_chunks = self.build_web_vtt_chunks(timeout, cue_buffers_to_process);

        let last_web_vtt_chunk = web_vtt_chunks.last().unwrap();

        for chunk in &web_vtt_chunks {
            if chunk.completed {
                let web_vtt_output_buffer = self.serialize_web_vtt_chunk(chunk)?;

                // Sending the webvtt chunk
                self.finish_buffer(web_vtt_output_buffer)?;

                self.move_to_next_web_vtt_chunk();
            }
        }

        if last_web_vtt_chunk.completed {
            // Reset the global chunk to start from none
            let mut state = self.state.lock().unwrap();

            state.web_vtt_chunk = WebVtt::new();
        } else {
            // Assign chunk to complete in next iteration
            let mut state = self.state.lock().unwrap();

            state.web_vtt_chunk = last_web_vtt_chunk.web_vtt.clone();
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn next_time(&self) -> Option<ClockTime> {
        let state = self.state.lock().unwrap();

        state.next_aggregation_chunk_time
    }

    fn negotiate(&self) -> bool {
        true
    }
}
