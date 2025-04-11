use gst::subclass::ElementMetadata;
use gst::{
    glib, prelude::*, subclass::prelude::*, Buffer, Caps, Clock, ClockTime, Event, EventView,
    FlowError,
};
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

    fn serialize_web_vtt_chunk(&self) -> Result<Buffer, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let settings = self.settings.lock().unwrap();

        let serialized_web_vtt = state.web_vtt_chunk.to_string();

        gst::info!(CAT, imp = self, "{:?}", serialized_web_vtt);

        let mut web_vtt_output_buffer = gst::Buffer::from_slice(serialized_web_vtt);
        let web_vtt_output_buffer_mutable =
            web_vtt_output_buffer.get_mut().ok_or(FlowError::Error)?;

        web_vtt_output_buffer_mutable.set_pts(state.current_web_vtt_chunk_start_time);
        web_vtt_output_buffer_mutable.set_duration(settings.target_duration);

        state.web_vtt_chunk.cues.clear();

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

    fn aggregate(&self, _timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();

        let current_web_vtt_chunk_end_time = state.current_web_vtt_chunk_end_time;
        let next_aggregation_chunk_time = state.next_aggregation_chunk_time;

        drop(state);

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

        let mut cues_beyond_current_chunk: Vec<VttCue> = Vec::new();

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
                return Ok(gst::FlowSuccess::Ok);
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

            if next_aggregation_chunk_time.is_none() {
                // First cue sets the first chunk start time
                self.move_to_next_web_vtt_chunk_from(current_cue_start_time);

                // Add the first cue to the current chunk
                self.add_web_vtt_cue_to_chunk(current_cue);

                // Stops reading buffers. Next time will be current_cue_start_time + target_duration
                return Ok(gst::FlowSuccess::Ok);
            }

            let current_cue_end_time = current_cue_start_time + current_cue_duration;

            if current_cue_start_time.nseconds() < current_web_vtt_chunk_end_time.nseconds() {
                if current_cue_end_time.nseconds() <= current_web_vtt_chunk_end_time.nseconds() {
                    // Cue is fully inside the current webvtt chunk
                    self.add_web_vtt_cue_to_chunk(current_cue);
                } else {
                    // Cue overlaps with the next target duration
                    let (current_new_cue, next_new_cue) = self.split_web_vtt_cue(
                        current_cue_start_time,
                        current_cue_end_time,
                        current_cue,
                    );

                    // Adding the first partial cue to the current chunk
                    self.add_web_vtt_cue_to_chunk(current_new_cue);

                    // Adding the second partial cue to next chunk
                    cues_beyond_current_chunk.push(next_new_cue);
                }
            } else {
                // Current web vtt start time doesn't belong to the current chuck
                // We publish the current chunk and add the current web vtt to be ready for next chunk
                cues_beyond_current_chunk.push(current_cue);
            };
        }

        if !cues_beyond_current_chunk.is_empty() {
            let web_vtt_output_buffer = self.serialize_web_vtt_chunk()?;

            // Move the clock range to next chunk
            self.move_to_next_web_vtt_chunk();

            // Adding cues beyond the current chunk to the next chunk
            self.add_web_vtt_cues_to_chunk(cues_beyond_current_chunk);

            // Sending the webvtt chunk
            return self.finish_buffer(web_vtt_output_buffer);
        } else {
            if sink.is_eos() {
                return Err(Eos);
            }
            
            let current_wall_clock = self.obj().clock().unwrap().time().unwrap();

            if current_wall_clock >= current_web_vtt_chunk_end_time {
                gst::info!(
                    CAT,
                    imp = self,
                    "current_wall_clock: {:?}, current_web_vtt_chunk_end_time: {:?}",
                    current_wall_clock,
                    current_web_vtt_chunk_end_time
                );

                let web_vtt_output_buffer = self.serialize_web_vtt_chunk()?;

                // Move the clock range to next chunk
                self.move_to_next_web_vtt_chunk();

                // Sending the webvtt chunk
                return self.finish_buffer(web_vtt_output_buffer);
            }
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
