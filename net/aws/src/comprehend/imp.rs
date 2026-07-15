// Copyright (C) 2026 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! AWS Comprehend element.
//!
//! This element calls AWS Comprehend to perform various comprehension tasks.
//!
//! The detection tasks to run are controlled through the detection-tasks property,
//! which is an array made up of structures with a "task-type" string field, the following
//! task types are currently supported:
//!
//! * detect-entities
//! * detect-key-phrases
//! * detect-pii-entities
//! * detect-sentiment
//! * detect-syntax
//! * detect-targeted-sentiment
//! * detect-toxic-content
//!
//! The tasks are run on parallel for each input buffer, and a custom AWSComprehendMeta
//! is added to the output buffer with the results.
//!
//! The structure of the meta contains:
//!
//! * A speaker-language field (string)
//! * A task-results field (array of structures)
//!
//! The content of the result structures varies depending on the task, and maps to
//! the fields documented in https://docs.aws.amazon.com/comprehend/latest/APIReference/API_Operations.html
//!
//! As most detection operations require a language code, the element must first
//! determine one, it does so as follows:
//!
//! * If the language property was set, it is used
//! * If the speaker-language-codes property was set, and the current speaker is known (through
//!   `rstranscribe/speaker-change` custom events) and listed in speaker-language-codes, its language code is
//!   used
//! * Otherwise dominant language detection is run using AWS comprehend
//!
//! A probation mechanism is implemented in order to optionally reduce API costs:
//!
//! * If the speaker-threshold property is set to a value N, we consider that the language for a speaker
//!   is permanent after N consecutive detections have successfully returned the same language code
//! * The score threshold for a successful detection can be set through the speaker-score-threshold
//!   property. If a detection scores lower than the threshold, the result is still used for further
//!   detection tasks, but doesn't affect probation.
//!
//! By default the element will systematically run dominant language detection.
//!
//! Finally, service errors can be ignored with `ignore-service-errors`. Further, more sophisticated
//! error handling may be implemented in the future.

use aws_sdk_comprehend::error::DisplayErrorContext;
use aws_sdk_comprehend::types::TextSegment;
use aws_sdk_s3::config::StalledStreamProtectionConfig;
use gio::glib::value::FromValue;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use futures::future::{AbortHandle, FutureExt, abortable};

use std::sync::Mutex;

use crate::s3utils::RUNTIME;
use anyhow::{Error, anyhow};

use std::collections::HashMap;
use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "awscomprehend",
        gst::DebugColorFlags::empty(),
        Some("AWS Comprehend element"),
    )
});

#[allow(deprecated)]
static AWS_BEHAVIOR_VERSION: LazyLock<aws_config::BehaviorVersion> =
    LazyLock::new(aws_config::BehaviorVersion::v2023_11_09);

const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_LATENCY_MS: u32 = 2_000;
const DEFAULT_SPEAKER_THRESHOLD: u32 = 0;
const DEFAULT_SPEAKER_SCORE_THRESHOLD: f32 = 0.;
const DEFAULT_IGNORE_SERVICE_ERRORS: bool = false;

#[derive(Debug)]
struct AWSComprehendServiceError {
    error: String,
}

impl std::fmt::Display for AWSComprehendServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for AWSComprehendServiceError {}

#[derive(Debug, Clone)]
pub(super) struct Settings {
    latency_ms: u32,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    detection_tasks: Vec<super::AwsComprehendDetectionType>,
    speaker_threshold: u32,
    speaker_score_threshold: f32,
    language_code: Option<String>,
    ignore_service_errors: bool,
    speakers: HashMap<Option<String>, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            latency_ms: DEFAULT_LATENCY_MS,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            detection_tasks: vec![],
            speaker_threshold: DEFAULT_SPEAKER_THRESHOLD,
            speaker_score_threshold: DEFAULT_SPEAKER_SCORE_THRESHOLD,
            language_code: None,
            ignore_service_errors: DEFAULT_IGNORE_SERVICE_ERRORS,
            speakers: HashMap::new(),
        }
    }
}

struct State {
    out_segment: gst::FormattedSegment<gst::ClockTime>,
    client: Option<aws_sdk_comprehend::Client>,
    send_abort_handle: Option<AbortHandle>,
    // (live, min, max)
    upstream_latency: Option<(bool, gst::ClockTime, Option<gst::ClockTime>)>,
    flushing: bool,

    current_speaker: Option<String>,
    probation_speakers: HashMap<Option<String>, (String, usize)>,
    speakers: HashMap<Option<String>, String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            out_segment: gst::FormattedSegment::new(),
            client: None,
            send_abort_handle: None,
            upstream_latency: None,
            flushing: false,
            current_speaker: None,
            probation_speakers: HashMap::new(),
            speakers: HashMap::new(),
        }
    }
}

pub struct Comprehend {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    pub(super) aws_config: Mutex<Option<aws_config::SdkConfig>>,
}

impl Comprehend {
    fn upstream_latency(&self) -> Option<(bool, gst::ClockTime, Option<gst::ClockTime>)> {
        if let Some(latency) = self.state.lock().unwrap().upstream_latency {
            return Some(latency);
        }

        let mut peer_query = gst::query::Latency::new();

        let ret = self.sinkpad.peer_query(&mut peer_query);

        if ret {
            let upstream_latency = peer_query.result();
            gst::info!(
                CAT,
                imp = self,
                "queried upstream latency: {upstream_latency:?}"
            );

            self.state.lock().unwrap().upstream_latency = Some(upstream_latency);

            Some(upstream_latency)
        } else {
            gst::trace!(CAT, imp = self, "could not query upstream latency");

            None
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::log!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                gst::info!(CAT, imp = self, "Received flush start, disconnecting");
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                self.disconnect();
                self.state.lock().unwrap().flushing = true;
                ret
            }
            FlushStop(_) => {
                let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                self.state.lock().unwrap().flushing = false;
                ret
            }
            Segment(e) => {
                let event = {
                    let desynchronized = self.settings.lock().unwrap().latency_ms == u32::MAX;
                    let upstream_latency = self.upstream_latency();

                    let mut state = self.state.lock().unwrap();

                    let segment = if desynchronized
                        && upstream_latency.map(|ul| ul.0).unwrap_or(false)
                    {
                        let mut segment = gst::FormattedSegment::new();

                        if let Some(position) = self.obj().current_running_time() {
                            segment.set_position(position);
                        }

                        segment
                    } else {
                        match e.segment().clone().downcast::<gst::ClockTime>() {
                            Err(segment) => {
                                gst::element_imp_error!(
                                    self,
                                    gst::StreamError::Format,
                                    ["Only Time segments supported, got {:?}", segment.format(),]
                                );
                                return false;
                            }
                            Ok(segment) => segment,
                        }
                    };

                    state.out_segment = segment.clone();

                    gst::debug!(CAT, imp = self, "stored segment {:?}", state.out_segment);
                    gst::event::Segment::new(&segment)
                };

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Gap(g) => {
                let desynchronized = self.settings.lock().unwrap().latency_ms == u32::MAX;
                let upstream_latency = self.upstream_latency();
                let (pts, duration) = g.get();

                let mut state = self.state.lock().unwrap();

                let new_gap_event = if let Some(position) = state.out_segment.position() {
                    if desynchronized && upstream_latency.map(|ul| ul.0).unwrap_or(false) {
                        self.obj().current_running_time().and_then(|now| {
                            if position < now {
                                Some(gst::event::Gap::new(position, now - position))
                            } else {
                                None
                            }
                        })
                    } else if let Some(duration) = duration {
                        let end_pts = pts + duration;

                        if end_pts > position {
                            // Output our own gap event that starts at our current position
                            Some(
                                gst::event::Gap::builder(position)
                                    .duration(end_pts - position)
                                    .seqnum(event.seqnum())
                                    .build(),
                            )
                        } else {
                            // We have already advanced past this gap's end
                            None
                        }
                    } else if pts > position {
                        Some(gst::event::Gap::builder(pts).seqnum(event.seqnum()).build())
                    } else {
                        // This duration-less gap was older that our current position, do
                        // nothing
                        None
                    }
                } else {
                    // Position wasn't set yet, the gap can be forwarded unchanged
                    Some(event.clone())
                };

                if let Some(ref event) = new_gap_event {
                    let Gap(gap) = event.view() else {
                        unreachable!()
                    };
                    let (new_pts, new_duration) = gap.get();

                    gst::log!(
                        CAT,
                        imp = self,
                        "pushing gap with pts {new_pts} and duration {new_duration:?}"
                    );

                    state.out_segment.set_position(match new_duration {
                        Some(new_duration) => new_duration + new_pts,
                        _ => new_pts,
                    });
                }

                drop(state);

                if let Some(event) = new_gap_event {
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                } else {
                    true
                }
            }
            CustomDownstream(c) => {
                let Some(s) = c.structure() else {
                    return gst::Pad::event_default(pad, Some(&*self.obj()), event);
                };

                if s.name().as_str() == "rstranscribe/speaker-change" {
                    let speaker = s.get::<String>("speaker").ok();
                    gst::debug!(CAT, imp = self, "speaker change: {speaker:?}");
                    self.state.lock().unwrap().current_speaker = speaker;
                }

                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn finish_buffer(
        &self,
        mut buffer: gst::Buffer,
        results: Vec<gst::Structure>,
        speaker_language: Option<String>,
    ) -> gst::Buffer {
        let desynchronized = self.settings.lock().unwrap().latency_ms == u32::MAX;
        let upstream_latency = self.upstream_latency();
        let mut pts = buffer.pts();
        let duration = buffer.duration();

        if desynchronized {
            if upstream_latency.map(|ul| ul.0).unwrap_or(false)
                && let Some(current_rtime) = self.obj().current_running_time()
            {
                pts = Some(current_rtime);

                gst::debug!(
                    CAT,
                    imp = self,
                    "adjusted pts to {pts:?}, now is {current_rtime}"
                );
            } else {
                gst::debug!(CAT, imp = self, "no current running time to adjust to");
            }
        }

        if let Some(pts) = pts {
            let position = match duration {
                Some(duration) => pts + duration,
                None => pts,
            };
            self.state
                .lock()
                .unwrap()
                .out_segment
                .set_position(position);
        }

        {
            let buf_mut = buffer.make_mut();
            buf_mut.set_pts(pts);

            if let Some(speaker_language) = speaker_language {
                if let Ok(mut m) = gst::meta::CustomMeta::add(buf_mut, "AWSComprehendMeta") {
                    let s_mut = m.mut_structure();
                    s_mut.set("speaker-language", speaker_language);
                    s_mut.set(
                        "task-results",
                        results
                            .iter()
                            .map(|s| s.to_send_value())
                            .collect::<gst::Array>(),
                    );
                    gst::log!(CAT, imp = self, "result meta: {:?}", s_mut);
                }
            }
        }

        buffer
    }

    async fn send(&self, buffer: gst::Buffer) -> Result<Option<gst::Buffer>, Error> {
        let content = self.read_buffer(&buffer).map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["{}", err]);
            gst::FlowError::Error
        })?;

        let (
            detection_tasks,
            speaker_threshold,
            speaker_score_threshold,
            default_language_code,
            ignore_service_errors,
        ) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.detection_tasks.clone(),
                settings.speaker_threshold,
                settings.speaker_score_threshold,
                settings.language_code.clone(),
                settings.ignore_service_errors,
            )
        };

        let (client, speaker_language) = {
            let state = self.state.lock().unwrap();

            let Some(client) = state.client.as_ref().cloned() else {
                return Ok(None);
            };

            let speaker_language = match default_language_code {
                Some(language_code) => Some(language_code),
                None => state.speakers.get(&state.current_speaker).cloned(),
            };

            if let Some(ref speaker_language) = speaker_language {
                gst::debug!(
                    CAT,
                    imp = self,
                    "language for speaker {:?}: {speaker_language}",
                    state.current_speaker
                );
            };

            (client, speaker_language)
        };

        let speaker_language = match speaker_language {
            Some(speaker_language) => speaker_language,
            None => {
                let job = client
                    .detect_dominant_language()
                    .text(content.clone())
                    .send();

                gst::debug!(
                    CAT,
                    imp = self,
                    "detecting dominant language on text {content}"
                );

                let resp = match job.await {
                    Ok(resp) => resp,
                    Err(err) => {
                        if ignore_service_errors && err.as_service_error().is_some() {
                            gst::warning!(
                                CAT,
                                "Failed detecting dominant language for {content}: {}, not running further tasks",
                                DisplayErrorContext(err)
                            );

                            return Ok(Some(self.finish_buffer(buffer, vec![], None)));
                        } else {
                            return Err(err.into());
                        }
                    }
                };

                if let Some((dominant_language, score)) = resp
                    .languages()
                    .first()
                    .and_then(|l| l.language_code.clone().zip(l.score()))
                {
                    let mut state = self.state.lock().unwrap();
                    let current_speaker = state.current_speaker.as_ref().cloned();
                    let mut probed_speaker_language = None;

                    gst::debug!(
                        CAT,
                        imp = self,
                        "detected dominant language for speaker {:?}: {}",
                        current_speaker,
                        dominant_language
                    );

                    if score > speaker_score_threshold {
                        if let Some(speaker_language) =
                            state.probation_speakers.get_mut(&current_speaker)
                        {
                            if speaker_language.0 == dominant_language {
                                speaker_language.1 += 1;
                                if speaker_threshold != 0
                                    && speaker_language.1 > speaker_threshold as usize
                                {
                                    probed_speaker_language = Some(speaker_language.0.clone());
                                }
                            } else {
                                speaker_language.0 = dominant_language.clone();
                                speaker_language.1 = 1;
                            }
                        } else {
                            state
                                .probation_speakers
                                .insert(current_speaker.clone(), (dominant_language.clone(), 1));
                        }

                        if let Some(speaker_language) = probed_speaker_language {
                            gst::debug!(
                                CAT,
                                imp = self,
                                "Taking speaker {current_speaker:?} our of probation with language {speaker_language}"
                            );
                            state.probation_speakers.remove(&current_speaker);
                            state.speakers.insert(current_speaker, speaker_language);
                        }

                        dominant_language
                    } else if let Some(speaker_language) =
                        state.probation_speakers.get(&current_speaker)
                    {
                        // Score is insufficient to update existing detection
                        speaker_language.0.clone()
                    } else {
                        // No existing detection
                        dominant_language
                    }
                } else {
                    return Ok(Some(self.finish_buffer(buffer, vec![], None)));
                }
            }
        };

        gst::debug!(
            CAT,
            imp = self,
            "calling comprehend on text {content} with language {speaker_language}"
        );

        let mut set = tokio::task::JoinSet::new();

        for task in detection_tasks {
            use super::AwsComprehendDetectionType::*;
            match task {
                DetectEntities => {
                    set.spawn(
                        client
                            .detect_entities()
                            .text(content.clone())
                            .language_code(speaker_language.as_str().into())
                            .send()
                            .map(|result| {
                                result
                                    .map(|result| {
                                        gst::Structure::builder("aws-comprehend-entities")
                                            .field(
                                                "entities",
                                                result
                                                    .entities()
                                                    .iter()
                                                    .map(|e| {
                                                        gst::Structure::builder("entity")
                                                            .field_if_some("score", e.score())
                                                            .field_if_some(
                                                                "type",
                                                                e.r#type().map(|t| t.to_string()),
                                                            )
                                                            .field_if_some("text", e.text())
                                                            .field_if_some(
                                                                "begin-offset",
                                                                e.begin_offset(),
                                                            )
                                                            .field_if_some(
                                                                "end-offset",
                                                                e.end_offset(),
                                                            )
                                                            .build()
                                                            .to_send_value()
                                                    })
                                                    .collect::<gst::Array>(),
                                            )
                                            .build()
                                    })
                                    .map_err(|err| {
                                        err.map_service_error(|e| AWSComprehendServiceError {
                                            error: e.to_string(),
                                        })
                                    })
                            }),
                    );
                }
                DetectKeyPhrases => {
                    set.spawn(
                        client
                            .detect_key_phrases()
                            .text(content.clone())
                            .language_code(speaker_language.as_str().into())
                            .send()
                            .map(|result| {
                                result
                                    .map(|result| {
                                        gst::Structure::builder("aws-comprehend-key-phrases")
                                            .field(
                                                "key-phrases",
                                                result
                                                    .key_phrases()
                                                    .iter()
                                                    .map(|k| {
                                                        gst::Structure::builder("key-phrase")
                                                            .field_if_some("score", k.score())
                                                            .field_if_some("text", k.text())
                                                            .field_if_some(
                                                                "begin_offset",
                                                                k.begin_offset(),
                                                            )
                                                            .field_if_some(
                                                                "end_offset",
                                                                k.end_offset(),
                                                            )
                                                            .build()
                                                            .to_send_value()
                                                    })
                                                    .collect::<gst::Array>(),
                                            )
                                            .build()
                                    })
                                    .map_err(|err| {
                                        err.map_service_error(|e| AWSComprehendServiceError {
                                            error: e.to_string(),
                                        })
                                    })
                            }),
                    );
                }
                DetectPiiEntities => {
                    set.spawn(
                        client
                            .detect_pii_entities()
                            .text(content.clone())
                            .language_code(speaker_language.as_str().into())
                            .send()
                            .map(|result| {
                                result
                                    .map(|result| {
                                        gst::Structure::builder("aws-comprehend-pii-entities")
                                            .field(
                                                "entities",
                                                result
                                                    .entities()
                                                    .iter()
                                                    .map(|e| {
                                                        gst::Structure::builder("entity")
                                                            .field_if_some("score", e.score())
                                                            .field_if_some(
                                                                "type",
                                                                e.r#type().map(|t| t.to_string()),
                                                            )
                                                            .field_if_some(
                                                                "begin-offset",
                                                                e.begin_offset(),
                                                            )
                                                            .field_if_some(
                                                                "end-offset",
                                                                e.end_offset(),
                                                            )
                                                            .build()
                                                            .to_send_value()
                                                    })
                                                    .collect::<gst::Array>(),
                                            )
                                            .build()
                                    })
                                    .map_err(|err| {
                                        err.map_service_error(|e| AWSComprehendServiceError {
                                            error: e.to_string(),
                                        })
                                    })
                            }),
                    );
                }
                DetectSentiment => {
                    set.spawn(
                        client
                            .detect_sentiment()
                            .text(content.clone())
                            .language_code(speaker_language.as_str().into())
                            .send()
                            .map(|result| {
                                result
                                    .map(|result| {
                                        gst::Structure::builder("aws-comprehend-sentiment")
                                            .field_if_some(
                                                "sentiment",
                                                result.sentiment().map(|s| s.to_string()),
                                            )
                                            .field_if_some(
                                                "sentiment-score",
                                                result.sentiment_score().map(|s| {
                                                    gst::Structure::builder("score")
                                                        .field_if_some("positive", s.positive())
                                                        .field_if_some("negative", s.negative())
                                                        .field_if_some("neutral", s.neutral())
                                                        .field_if_some("mixed", s.mixed())
                                                        .build()
                                                }),
                                            )
                                            .build()
                                    })
                                    .map_err(|err| {
                                        err.map_service_error(|e| AWSComprehendServiceError {
                                            error: e.to_string(),
                                        })
                                    })
                            }),
                    );
                }
                DetectSyntax => {
                    set.spawn(
                        client
                            .detect_syntax()
                            .text(content.clone())
                            .language_code(speaker_language.as_str().into())
                            .send()
                            .map(|result| {
                                result
                                    .map(|result| {
                                        gst::Structure::builder("aws-comprehend-syntax")
                                            .field(
                                                "syntax-tokens",
                                                result
                                                    .syntax_tokens()
                                                    .iter()
                                                    .map(|t| {
                                                        gst::Structure::builder("token")
                                                            .field_if_some("token-id", t.token_id())
                                                            .field_if_some("text", t.text())
                                                            .field_if_some(
                                                                "begin-offset",
                                                                t.begin_offset(),
                                                            )
                                                            .field_if_some(
                                                                "end-offset",
                                                                t.end_offset(),
                                                            )
                                                            .field_if_some(
                                                                "part-of-speech",
                                                                t.part_of_speech().map(|tag| {
                                                                    gst::Structure::builder("tag")
                                                                        .field_if_some(
                                                                            "tag",
                                                                            tag.tag().map(|tag| {
                                                                                tag.to_string()
                                                                            }),
                                                                        )
                                                                        .field_if_some(
                                                                            "score",
                                                                            tag.score(),
                                                                        )
                                                                        .build()
                                                                }),
                                                            )
                                                            .build()
                                                            .to_send_value()
                                                    })
                                                    .collect::<gst::Array>(),
                                            )
                                            .build()
                                    })
                                    .map_err(|err| {
                                        err.map_service_error(|e| AWSComprehendServiceError {
                                            error: e.to_string(),
                                        })
                                    })
                            }),
                    );
                }
                DetectTargetedSentiment => {
                    set.spawn(
                        client
                            .detect_targeted_sentiment()
                            .text(content.clone())
                            .language_code(speaker_language.as_str().into())
                            .send()
                            .map(|result| {
                                result.map(|result| {
                                    gst::Structure::builder("aws-comprehend-targeted-sentiment")
                                        .field("entities", result.entities.unwrap_or(vec![]).iter().map(|e| {
                                            gst::Structure::builder("entity")
                                                .field("descriptive_mention_index", e.descriptive_mention_index().iter().map(|idx| idx.to_send_value()).collect::<gst::Array>())
                                                .field("mentions", e.mentions().iter().map(|m| {
                                                    gst::Structure::builder("mention")
                                                        .field_if_some("score", m.score())
                                                        .field_if_some("group-score", m.group_score())
                                                        .field_if_some("text", m.text())
                                                        .field_if_some("type", m.r#type().map(|t| t.to_string()))
                                                        .field_if_some("mention-sentiment", m.mention_sentiment().map(|ms| {
                                                            gst::Structure::builder("mention-sentiment")
                                                                .field_if_some("sentiment", ms.sentiment().map(|s| s.to_string()))
                                                                .field_if_some("sentiment-score", ms.sentiment_score().map(|s| {
                                                                    gst::Structure::builder("score")
                                                                        .field_if_some("positive", s.positive())
                                                                        .field_if_some("negative", s.negative())
                                                                        .field_if_some("neutral", s.neutral())
                                                                        .field_if_some("mixed", s.mixed())
                                                                        .build()
                                                                }))
                                                                .build()
                                                        }))
                                                        .build()
                                                        .to_send_value()
                                                })
                                                .collect::<gst::Array>())
                                                .build()
                                                .to_send_value()
                                        })
                                        .collect::<gst::Array>())
                                        .build()
                                }).map_err(|err| {
                                    err.map_service_error(|e| AWSComprehendServiceError { error: e.to_string() })
                                })
                            }),
                    );
                }
                DetectToxicContent => {
                    let Ok(text_segment) = TextSegment::builder().text(content.clone()).build()
                    else {
                        gst::error!(
                            CAT,
                            imp = self,
                            "Failed to build text segment from text {content}"
                        );
                        continue;
                    };
                    set.spawn(
                        client
                            .detect_toxic_content()
                            .text_segments(text_segment)
                            .language_code("en".into())
                            .send()
                            .map(|result| {
                                result
                                    .map(|result| {
                                        gst::Structure::builder("aws-comprehend-toxic-sentiment")
                                            .field(
                                                "result-list",
                                                result
                                                    .result_list()
                                                    .iter()
                                                    .map(|l| {
                                                        gst::Structure::builder("labels")
                                                            .field(
                                                                "labels",
                                                                l.labels()
                                                                    .iter()
                                                                    .map(|l| {
                                                                        gst::Structure::builder(
                                                                            "label",
                                                                        )
                                                                        .field_if_some(
                                                                            "name",
                                                                            l.name().map(|n| {
                                                                                n.to_string()
                                                                            }),
                                                                        )
                                                                        .field_if_some(
                                                                            "score",
                                                                            l.score(),
                                                                        )
                                                                        .build()
                                                                        .to_send_value()
                                                                    })
                                                                    .collect::<gst::Array>(),
                                                            )
                                                            .field_if_some("toxicity", l.toxicity())
                                                            .build()
                                                            .to_send_value()
                                                    })
                                                    .collect::<gst::Array>(),
                                            )
                                            .build()
                                    })
                                    .map_err(|err| {
                                        err.map_service_error(|e| AWSComprehendServiceError {
                                            error: e.to_string(),
                                        })
                                    })
                            }),
                    );
                }
            };
        }

        let result = set.join_all().await;

        let errors: Vec<_> = result
            .iter()
            .filter_map(|r| match r {
                Ok(_) => None,
                Err(e) => {
                    if ignore_service_errors && e.as_service_error().is_some() {
                        gst::warning!(CAT, "Ignoring service error: {}", DisplayErrorContext(e));
                        None
                    } else {
                        Some(DisplayErrorContext(e).to_string())
                    }
                }
            })
            .collect();

        if !errors.is_empty() {
            return Err(anyhow!("one or more task failed: {errors:?}"));
        }

        Ok(Some(
            self.finish_buffer(
                buffer,
                result
                    .iter()
                    .filter_map(|r| r.as_ref().cloned().ok())
                    .collect(),
                Some(speaker_language),
            ),
        ))
    }

    fn do_send(&self, buffer: gst::Buffer) -> Result<Option<gst::Buffer>, gst::FlowError> {
        self.ensure_connection().map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Streaming failed: {err}"]);
            gst::FlowError::Error
        })?;

        let (future, abort_handle) = abortable(self.send(buffer));

        self.state.lock().unwrap().send_abort_handle = Some(abort_handle);

        match RUNTIME.block_on(future) {
            Err(_) => {
                gst::debug!(CAT, imp = self, "send aborted, returning flushing");
                Err(gst::FlowError::Flushing)
            }
            Ok(res) => match res {
                Err(e) => {
                    if !self.state.lock().unwrap().flushing {
                        gst::error!(CAT, imp = self, "Failed sending data: {e}");
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Failed,
                            ["Failed sending data: {}", e]
                        );
                        Err(gst::FlowError::Error)
                    } else {
                        Err(gst::FlowError::Flushing)
                    }
                }
                Ok(buf) => Ok(buf),
            },
        }
    }

    fn read_buffer(&self, buffer: &gst::Buffer) -> Result<String, Error> {
        let data = buffer
            .map_readable()
            .map_err(|_| anyhow!("Can't map buffer readable"))?;

        let data =
            std::str::from_utf8(&data).map_err(|err| anyhow!("Can't decode utf8: {}", err))?;

        Ok(data.to_owned())
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling {buffer:?}");

        let Some(outbuf) = self.do_send(buffer)? else {
            return Ok(gst::FlowSuccess::Ok);
        };

        self.srcpad.push(outbuf)
    }

    fn ensure_connection(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        if state.client.is_none() {
            state.client = Some(aws_sdk_comprehend::Client::new(
                self.aws_config.lock().unwrap().as_ref().expect("prepared"),
            ));
        }
        Ok(())
    }

    fn prepare(&self) -> Result<(), gst::ErrorMessage> {
        gst::debug!(CAT, imp = self, "Preparing");

        let (access_key, secret_access_key, session_token) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.access_key.clone(),
                settings.secret_access_key.clone(),
                settings.session_token.clone(),
            )
        };

        gst::info!(CAT, imp = self, "Loading aws config...");

        let config = RUNTIME.block_on(async move {
            let config_loader = match (access_key, secret_access_key) {
                (Some(key), Some(secret_key)) => {
                    gst::debug!(CAT, imp = self, "Using settings credentials");
                    aws_config::defaults(*AWS_BEHAVIOR_VERSION).credentials_provider(
                        aws_sdk_comprehend::config::Credentials::new(
                            key,
                            secret_key,
                            session_token,
                            None,
                            "translate",
                        ),
                    )
                }
                _ => {
                    gst::debug!(CAT, imp = self, "Attempting to get credentials from env...");
                    aws_config::defaults(*AWS_BEHAVIOR_VERSION)
                }
            };

            let config_loader = config_loader.region(
                aws_config::meta::region::RegionProviderChain::default_provider()
                    .or_else(DEFAULT_REGION),
            );

            let config_loader =
                config_loader.stalled_stream_protection(StalledStreamProtectionConfig::disabled());

            config_loader.load().await
        });
        gst::debug!(CAT, imp = self, "Using region {}", config.region().unwrap());

        *self.aws_config.lock().unwrap() = Some(config);

        gst::debug!(CAT, imp = self, "Prepared");

        Ok(())
    }

    fn disconnect(&self) {
        gst::info!(CAT, imp = self, "Disconnecting");
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();

        if let Some(abort_handle) = state.send_abort_handle.take() {
            abort_handle.abort();
        }

        *state = State::default();
        state.speakers = settings.speakers.clone();
        gst::info!(CAT, imp = self, "Disconnected");
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        gst::trace!(CAT, obj = pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Latency(ref mut q) => {
                let mut peer_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut peer_query);

                if ret {
                    let (live, min, max) = peer_query.result();
                    let latency_ms = self.settings.lock().unwrap().latency_ms;

                    let (live, our_latency) = if latency_ms == u32::MAX {
                        (true, gst::ClockTime::ZERO)
                    } else {
                        (live, gst::ClockTime::from_mseconds(latency_ms as u64))
                    };

                    if live {
                        q.set(true, min + our_latency, max.map(|max| max + our_latency));
                    } else {
                        q.set(live, min, max);
                    }
                }
                ret
            }
            gst::QueryViewMut::Position(ref mut q) => {
                if q.format() == gst::Format::Time {
                    let state = self.state.lock().unwrap();
                    q.set(
                        state
                            .out_segment
                            .to_stream_time(state.out_segment.position()),
                    );
                    true
                } else {
                    false
                }
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Comprehend {
    const NAME: &'static str = "GstAwsComprehend";
    type Type = super::Comprehend;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.sink_event(pad, event))
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::PadBuilder::<gst::Pad>::from_template(&templ)
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.src_query(pad, query))
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Default::default(),
            state: Default::default(),
            aws_config: Default::default(),
        }
    }
}

impl ObjectImpl for Comprehend {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("latency")
                    .nick("Latency")
                    .blurb("Amount of milliseconds to allow AWS Comprehend")
                    .default_value(DEFAULT_LATENCY_MS)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("access-key")
                    .nick("Access Key")
                    .blurb("AWS Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("secret-access-key")
                    .nick("Secret Access Key")
                    .blurb("AWS Secret Access Key")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-token")
                    .nick("Session Token")
                    .blurb("AWS temporary Session Token from STS")
                    .mutable_ready()
                    .build(),
                gst::ParamSpecArray::builder("detection-tasks")
                    .nick("Detection Tasks")
                    .blurb("The detection tasks to run")
                    .element_spec(
                        &glib::ParamSpecBoxed::builder::<gst::Structure>("detection-task")
                            .nick("Detection Task")
                            .blurb("A detection task")
                            .build(),
                    )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("speaker-threshold")
                    .nick("Speaker Threshold")
                    .blurb("Control after how many language detections for a given speaker to stop calling AWS comprehend")
                    .default_value(DEFAULT_SPEAKER_THRESHOLD)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFloat::builder("speaker-score-threshold")
                    .nick("Speaker Score Threshold")
                    .blurb("Control the minimum score to consider when probing the language for a speaker")
                    .default_value(DEFAULT_SPEAKER_SCORE_THRESHOLD)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("language-code")
                    .nick("Language Code")
                    .blurb("When set, overrides all language detection")
                    .mutable_ready()
                    .default_value(None)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("speaker-language-codes")
                    .nick("Speaker Language Codes")
                    .blurb("When set, overrides language detection for the given speakers")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("ignore-service-errors")
                    .nick("Ignore Service Errors")
                    .blurb("When set, only non-service SDK errors (eg credentials) will cause an error")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "latency" => {
                let mut settings = self.settings.lock().unwrap();
                settings.latency_ms = value.get::<u32>().expect("type checked upstream");
            }
            "access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "session-token" => {
                let mut settings = self.settings.lock().unwrap();
                settings.session_token = value.get().expect("type checked upstream");
            }
            "detection-tasks" => {
                let mut settings = self.settings.lock().unwrap();
                settings.detection_tasks = vec![];
                let tasks: gst::Array = value.get().expect("type checked upstream");
                let enum_class =
                    glib::EnumClass::with_type(super::AwsComprehendDetectionType::static_type())
                        .unwrap();
                for task in tasks.as_slice() {
                    let Some(s) = task
                        .get::<Option<gst::Structure>>()
                        .expect("type checked upstream")
                    else {
                        continue;
                    };

                    let Some(task_type) = s
                        .get::<String>("task-type")
                        .ok()
                        .and_then(|nick| enum_class.value_by_nick(&nick))
                    else {
                        gst::error!(CAT, imp = self, "need valid task type");
                        continue;
                    };

                    let task_type = unsafe {
                        super::AwsComprehendDetectionType::from_value(
                            &task_type.to_value(&enum_class),
                        )
                    };

                    settings.detection_tasks.push(task_type);
                }
            }
            "speaker-threshold" => {
                self.settings.lock().unwrap().speaker_threshold =
                    value.get().expect("type checked upstream");
            }
            "speaker-score-threshold" => {
                self.settings.lock().unwrap().speaker_score_threshold =
                    value.get().expect("type checked upstream");
            }
            "language-code" => {
                self.settings.lock().unwrap().language_code =
                    value.get().expect("type checked upstream");
            }
            "speaker-language-codes" => {
                let mut settings = self.settings.lock().unwrap();
                let mut state = self.state.lock().unwrap();
                state.speakers = HashMap::new();
                settings.speakers = HashMap::new();
                let s: gst::Structure = value.get().expect("type checked upstream");

                for (key, value) in s.iter() {
                    let Some(language_code) = value.get::<String>().ok() else {
                        gst::error!(CAT, imp = self, "need valid language code");
                        continue;
                    };

                    state
                        .speakers
                        .insert(Some(key.to_string()), language_code.clone());
                    settings
                        .speakers
                        .insert(Some(key.to_string()), language_code);
                }
            }
            "ignore-service-errors" => {
                self.settings.lock().unwrap().ignore_service_errors =
                    value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => {
                let settings = self.settings.lock().unwrap();
                settings.latency_ms.to_value()
            }
            "access-key" => {
                let settings = self.settings.lock().unwrap();
                settings.access_key.to_value()
            }
            "secret-access-key" => {
                let settings = self.settings.lock().unwrap();
                settings.secret_access_key.to_value()
            }
            "session-token" => {
                let settings = self.settings.lock().unwrap();
                settings.session_token.to_value()
            }
            "detection-tasks" => {
                let settings = self.settings.lock().unwrap();
                let mut tasks = vec![];
                let enum_class =
                    glib::EnumClass::with_type(super::AwsComprehendDetectionType::static_type())
                        .unwrap();
                for task_type in &settings.detection_tasks {
                    let mut s = gst::Structure::new_empty("detection-task");
                    let nick = enum_class.value(*task_type as i32).unwrap().nick();
                    s.set("task-type", nick);
                    tasks.push(s);
                }
                gst::Array::new(tasks).to_value()
            }
            "speaker-threshold" => self.settings.lock().unwrap().speaker_threshold.to_value(),
            "speaker-score-threshold" => self
                .settings
                .lock()
                .unwrap()
                .speaker_score_threshold
                .to_value(),
            "language-code" => self.settings.lock().unwrap().language_code.to_value(),
            "speaker-language-codes" => {
                let state = self.state.lock().unwrap();

                let mut s_builder = gst::Structure::builder("speakers");

                for (speaker, language_code) in &state.speakers {
                    if let Some(speaker) = speaker {
                        s_builder = s_builder.field(speaker, language_code);
                    }
                }
                s_builder.build().to_value()
            }
            "ignore-service-errors" => self
                .settings
                .lock()
                .unwrap()
                .ignore_service_errors
                .to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for Comprehend {}

impl ElementImpl for Comprehend {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Comprehend",
                "Text/Filter",
                "Text to Text + meta filter, using AWS comprehend",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("text/x-raw")
                        .field("format", "utf8")
                        .build(),
                )
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder_full()
                .structure(
                    gst::Structure::builder("text/x-raw")
                        .field("format", "utf8")
                        .build(),
                )
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
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
        gst::info!(CAT, imp = self, "Changing state {transition:?}");

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare().map_err(|err| {
                    self.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PausedToReady => {
                self.disconnect();
            }
            _ => (),
        }

        let mut success = self.parent_change_state(transition)?;

        let desynchronized = self.settings.lock().unwrap().latency_ms == u32::MAX;

        if desynchronized {
            match transition {
                gst::StateChange::ReadyToPaused => {
                    success = gst::StateChangeSuccess::NoPreroll;
                }
                gst::StateChange::PlayingToPaused => {
                    success = gst::StateChangeSuccess::NoPreroll;
                }
                _ => (),
            }
        }

        Ok(success)
    }

    fn provide_clock(&self) -> Option<gst::Clock> {
        Some(gst::SystemClock::obtain())
    }
}
