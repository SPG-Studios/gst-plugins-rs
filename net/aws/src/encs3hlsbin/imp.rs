// Copyright (C) 2022, Daily
//      Author: Rajneesh Soni <rajneesh@daily.co>
//      Author: Arun Raghavan <arun@asymptotic.io>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use aws_config::meta::region::RegionProviderChain;
use aws_sdk_s3::{
    config::{Credentials, Region},
    primitives::ByteStream,
    Client, Error,
};
use gst::element_imp_error;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use m3u8_rs::{MasterPlaylist, MediaPlaylistType, Resolution, VariantStream};
use once_cell::sync::Lazy;
use std::sync::Mutex;

const HLS_AUDIO_IN_QUEUE: &str = "hls-audio-in-queue";
const HLS_VIDEO_IN_TEE: &str = "hls-video-in-tee";
const HLS_AUDIO_PAD_NAME: &str = "hls_audio";
const HLS_VIDEO_PAD_NAME: &str = "hls_video";
const AUDIO_ENCODER_OUT_TEE: &str = "audio-enc-out-tee";
const HLS_SINK: &str = "hls-sink";

const MASTER_M3U8_NAME: &str = "master.m3u8";
const KEY_FRAME_INTERVAL_SEC: u32 = 2;

const DEFAULT_TARGET_DURATION: u32 = 6;
const DEFAULT_PLAYLIST_LENGTH: u32 = 5;
const DEFAULT_PREFIX: &str = "hlsout";
const DEFAULT_AUDIO_BITRATE: i32 = 128000;

const SIGNAL_OVERRUN: &str = "overrun";

/// Multivariant HLS to S3 Bin
/// Bin to generate HLS with multiple video bitrates and directly upload to S3. Required bitrates are specified with variant property.
/// Bin can generate audio-only, video-only and audio+video HLS.
///
/// # Examples pipeline
///
/// gst-launch-1.0 -vvv videotestsrc is-live=1 ! video/x-raw,format=I420,framerate=30/1,width=1920,height=1080 ! queue  !  sink.  \
/// audiotestsrc is-live=1 !  queue !  audio/x-raw ! queue ! sink. \
/// encs3hlsbin name=sink s3bucket=S3_BUCKET s3region=S3_REGION s3key-prefix=hls-test \
/// variants="<\"p1,width=(int)1280,height=(int)720,fps=(int)20,bitrate=(int)3000\",\"p2,width=(int)640,height=(int)360,fps=(int)20,bitrate=(int)2000\" >"
///
/// Pipeline -
/// [videoQueue]->[tee]->[queue]->[videoConvert]->[videoScale]->[videoRate]->[capsfilter]->[x264enc]->[h264parse]----
///                                                                                                                  |
///                                                                                                                   ->
///                                                                                                                      [s3hlssink]
///                                                                                                                   ->
///                                                                                                                   |
/// [audioQueue]->[audioConvert]->[audioResample]->[capsfilter]->[avenc_aac]->[aacparse]->[tee]->[queue]---------------
///
/// TODO List
/// allow audio variants properties, to set audio at various bitrates
/// allow adding/removing video variants dynamically
/// allow adding muxer from application to generate fMP4, currently mpeg-ts is the only option
/// reorganize code to allow different sink (not only s3)
/// ability to generate encrypted hls
///

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "encs3hlsbin",
        gst::DebugColorFlags::empty(),
        Some("Multivariant HLS to S3 Bin"),
    )
});

//TODO: allow setting 'encoder profile': baseline,main,high
#[derive(Debug, Clone)]
struct VariantProps {
    width: i32,
    height: i32,
    fps: i32,
    bitrate: i32,
    iframe_only: bool,
}

#[derive(Debug, Clone)]
struct Settings {
    variants: Vec<VariantProps>,
    target_duration: u32,
    playlist_len: u32,
    prefix: String,
    playlist_type: Option<MediaPlaylistType>,
    s3bucket: Option<String>,
    s3region: Option<String>,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    audio_queue: Option<gst::Element>,
    video_in_tee: Option<gst::Element>,
    s3_hls_sinks: Vec<gst::Element>,
    video_sink: bool,
    audio_sink: bool,
}

impl From<gst::Structure> for VariantProps {
    fn from(s: gst::Structure) -> Self {
        VariantProps {
            width: s.get("width").expect("width missing in variant props"),
            height: s.get("height").expect("height missing in variant props"),
            fps: s.get("fps").expect("fps missing in variant props"),
            bitrate: s.get("bitrate").expect("bitrate missing in variant props"),
            iframe_only: s.get("iframe-only").unwrap_or(false),
        }
    }
}

impl From<VariantProps> for gst::Structure {
    fn from(obj: VariantProps) -> Self {
        gst::Structure::builder("variant-params")
            .field("width", obj.width)
            .field("height", obj.height)
            .field("fps", obj.fps)
            .field("bitrate", obj.bitrate)
            .field("iframe-only", obj.iframe_only)
            .build()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            variants: vec![VariantProps {
                width: 960,
                height: 540,
                fps: 30,
                bitrate: 2000,
                iframe_only: false,
            }],
            target_duration: DEFAULT_TARGET_DURATION,
            playlist_len: DEFAULT_PLAYLIST_LENGTH,
            prefix: String::from(DEFAULT_PREFIX),
            playlist_type: None,
            s3bucket: None,
            s3region: None,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            audio_queue: None,
            video_in_tee: None,
            s3_hls_sinks: Vec::new(),
            audio_sink: false,
            video_sink: false,
        }
    }
}

#[derive(Default)]
pub struct EncS3HlsBin {
    settings: Mutex<Settings>,
}

// media-manifest path is relative in master m3u8
// hlssink3 playlist-location require complete path
fn get_media_manifest_name(bitrate: &str, prefix: &str, is_master: bool) -> String {
    if is_master {
        format!("{bitrate}/media-{bitrate}.m3u8", bitrate = bitrate)
    } else {
        format!(
            "{prefix}/{bitrate}/media-{bitrate}.m3u8",
            prefix = prefix,
            bitrate = bitrate
        )
    }
}

fn get_media_segment_name(bitrate: &str, prefix: &str) -> String {
    let media_segment_name = format!(
        "{prefix}/{bitrate}/segment%05d.ts",
        prefix = prefix,
        bitrate = bitrate
    );
    media_segment_name
}

fn generate_master_m3u8(variants: &[VariantProps], prefix: &str, enable_video: bool) -> String {
    //TODO: codecs string is optional but recommended
    //it should be probed from caps flow

    let media_variants = if enable_video {
        variants
            .iter()
            .map(|v| VariantStream {
                is_i_frame: v.iframe_only,
                average_bandwidth: Some(v.bitrate as u64),
                uri: get_media_manifest_name(&v.bitrate.to_string(), prefix, true),
                bandwidth: v.bitrate as u64,
                resolution: Some(Resolution {
                    width: v.width as u64,
                    height: v.height as u64,
                }),
                frame_rate: if v.iframe_only {
                    None
                } else {
                    Some(v.fps as f64)
                },
                ..Default::default()
            })
            .collect()
    } else {
        vec![VariantStream {
            average_bandwidth: Some(DEFAULT_AUDIO_BITRATE as u64),
            uri: get_media_manifest_name(&DEFAULT_AUDIO_BITRATE.to_string(), prefix, true),
            bandwidth: DEFAULT_AUDIO_BITRATE as u64,
            audio: Some("mp4a.40.2".to_string()),
            ..Default::default()
        }]
    };

    let playlist = MasterPlaylist {
        version: Some(4),
        variants: media_variants,
        ..Default::default()
    };
    let mut master_m3u8: Vec<u8> = Vec::new();
    playlist
        .write_to(&mut master_m3u8)
        .expect("Failed to write playlist");
    String::from_utf8(master_m3u8).expect("Playlist to string conversion failed")
}

async fn upload_master_m3u8_to_s3(settings: &Settings, m3u8_data: String) -> Result<(), Error> {
    let s3region = settings.s3region.clone();
    let m3u8_key = format!("{}/{}", settings.prefix, MASTER_M3U8_NAME);
    let region_provider = RegionProviderChain::first_try(s3region.map(Region::new))
        .or_default_provider()
        .or_else(Region::new("us-west-2"));

    let shared_config = match (
        settings.access_key.as_ref(),
        settings.secret_access_key.as_ref(),
    ) {
        (Some(access_key), Some(secret_access_key)) => {
            let creds = Credentials::new(
                access_key.clone(),
                secret_access_key.clone(),
                settings.session_token.clone(),
                None,
                "gst-plugins-rs",
            );
            aws_config::from_env()
                .region(region_provider)
                .credentials_provider(creds)
                .load()
                .await
        }
        _ => aws_config::from_env().region(region_provider).load().await,
    };

    let client = Client::new(&shared_config);
    // upload
    let body = ByteStream::from(m3u8_data.into_bytes());

    client
        .put_object()
        .bucket(settings.s3bucket.as_ref().expect("Bucket must be set"))
        .key(m3u8_key)
        .body(body)
        .send()
        .await?;

    gst::debug!(CAT, "master.m3u8 uploaded",);

    Ok(())
}

impl EncS3HlsBin {
    fn setup_queue(&self, queue: &gst::Element) {
        queue.set_property("max-size-bytes", 0u32);
        queue.set_property("max-size-buffers", 0u32);
        queue.set_property("max-size-time", 5 * gst::ClockTime::SECOND);

        let element_weak = self.obj().downgrade();
        queue.connect("overrun", false, move |args| {
            let element = match element_weak.upgrade() {
                Some(element) => element,
                None => return None,
            };

            let queue = args[0]
                .get::<gst::Element>()
                .expect("First argument to overrun must be a queue");
            element.emit_by_name::<()>(SIGNAL_OVERRUN, &[&queue]);

            None
        });
    }

    fn create_stats(&self, settings: &Settings) -> gst::Structure {
        let s3_hls_sinks = settings.s3_hls_sinks.clone();

        let mut stats = gst::Structure::builder("stats").build();

        for sink in s3_hls_sinks.iter() {
            let sink_stats = sink.property::<gst::Structure>("stats");
            stats.set_value(&sink.name(), sink_stats.to_send_value());
        }

        stats
    }

    fn make_and_add(&self, hlsbin: &str, el_id: &str) -> gst::Element {
        let obj = self.obj();
        let el_name = format!("{}-{}-{}", HLS_SINK, el_id, hlsbin);
        let el = gst::ElementFactory::make(hlsbin)
            .name(&el_name)
            .build()
            .expect("hlsbin should be available");
        let err_str = format!("Failed to add {}", hlsbin);

        obj.add(&el).expect(&err_str);

        el
    }

    fn create_audio_encoder_pipeline(
        &self,
        audio_enc_tee: &gst::Element,
        audio_queue: &gst::Element,
    ) {
        // queue ! audioconvert ! audiorate ! capsfilter ! audio_encoder ! aacparse !  ....
        let aconv = self.make_and_add("audioconvert", "audio");
        let arate = self.make_and_add("audioresample", "audio");
        let capsf = self.make_and_add("capsfilter", "audio");
        capsf.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("channels", 2i32)
                .field("rate", 48000i32)
                .field("format", "F32LE")
                .build(),
        );
        let aenc = self.make_and_add("avenc_aac", "audio");
        aenc.set_property("bitrate", DEFAULT_AUDIO_BITRATE);

        let aacparse = self.make_and_add("aacparse", "audio");
        // Now link all of them
        audio_queue
            .link(&aconv)
            .expect("Failed to link audio_queue <-> audioconvert");
        aconv
            .link(&arate)
            .expect("failed to link audioconvert <-> audiorate");
        arate
            .link(&capsf)
            .expect("Failed to link audiorate <-> capsf");
        capsf
            .link(&aenc)
            .expect("Failed to link capsf <-> avenc_aac");
        aenc.link(&aacparse)
            .expect("Failed to link avenc_aac <-> aacparse");
        aacparse
            .link(audio_enc_tee)
            .expect("Failed to link aacparse <-> tee");
    }

    fn create_video_encoder_pipeline(
        &self,
        variant_props: &VariantProps,
        target_duration: u32,
        video_in_tee: &gst::Element,
    ) -> gst::Element {
        // tee ! queue ! videoconvert ! videoscale ! videorate ! capsfilter ! x264enc -> h264parse
        let el_id = String::from("video") + &variant_props.bitrate.to_string();
        let vqueue = self.make_and_add("queue", &el_id);
        let vconv = self.make_and_add("videoconvert", &el_id);
        let vscale = self.make_and_add("videoscale", &el_id);
        let vrate = self.make_and_add("videorate", &el_id);
        let capsf = self.make_and_add("capsfilter", &el_id);

        let fps = if variant_props.iframe_only {
            gst::Fraction::new(1, target_duration as i32)
        } else {
            gst::Fraction::new(variant_props.fps, 1)
        };
        capsf.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", variant_props.width)
                .field("height", variant_props.height)
                .field("framerate", fps)
                .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                .field("format", "I420")
                .build(),
        );
        let venc = self.make_and_add("x264enc", &el_id);
        venc.set_property("bitrate", variant_props.bitrate as u32);
        venc.set_property_from_str("tune", "zerolatency");
        venc.set_property_from_str("speed-preset", "veryfast");
        if variant_props.iframe_only {
            venc.set_property("key-int-max", 1u32);
        } else {
            venc.set_property(
                "key-int-max",
                (variant_props.fps as u32) * KEY_FRAME_INTERVAL_SEC,
            );
        }

        self.setup_queue(&vqueue);

        let h264parse = self.make_and_add("h264parse", &el_id);

        video_in_tee
            .link(&vqueue)
            .expect("Failed to link video_in_tee <-> queue");
        vqueue
            .link(&vconv)
            .expect("Failed to link queue <-> videoconvert");
        vconv
            .link(&vrate)
            .expect("Failed to link videoconvert <-> videorate");
        vrate
            .link(&vscale)
            .expect("Failed to link videorate <-> videoscale");
        vscale
            .link(&capsf)
            .expect("Failed to link videoscale <-> capsfilter");
        capsf
            .link(&venc)
            .expect("failed to link capsfilter <-> videoenc");
        venc.link(&h264parse)
            .expect("Failed to link videoenc <-> h264parse");

        h264parse
    }

    fn create_and_link_hlssink(
        &self,
        h264parse: Option<gst::Element>,
        audio_enc_tee: Option<&gst::Element>,
        settings: &mut Settings,
        bitrate: &str,
        i_frames_only: bool,
    ) {
        /*
            -> aud_tee      -----> |
                                    ---->hlssink
            -> h264parse    -----> |
        */
        let el_id = String::from("mux") + bitrate;
        let s3hlssink = self.make_and_add("awss3hlssink", &el_id);

        s3hlssink.set_property("bucket", &settings.s3bucket);
        s3hlssink.set_property("region", &settings.s3region);
        if let Some(access_key) = settings.access_key.as_ref() {
            s3hlssink.set_property("access-key", access_key);
        }
        if let Some(secret_access_key) = settings.secret_access_key.as_ref() {
            s3hlssink.set_property("secret-access-key", secret_access_key);
        }
        if let Some(session_token) = settings.session_token.as_ref() {
            s3hlssink.set_property("session-token", session_token);
        }

        let hlssink = s3hlssink
            .property_value("hlssink")
            .get::<gst::Element>()
            .expect("Failed to get hlssink element via get_property");
        hlssink.set_property("target-duration", settings.target_duration);
        hlssink.set_property("playlist-length", settings.playlist_len);
        hlssink.set_property(
            "playlist-location",
            get_media_manifest_name(bitrate, &settings.prefix, false),
        );
        hlssink.set_property(
            "location",
            get_media_segment_name(bitrate, &settings.prefix),
        );

        if hlssink
            .factory()
            .expect("Factory name should be present")
            .name()
            .contains("hlssink3")
        {
            match &settings.playlist_type {
                Some(MediaPlaylistType::Vod) => {
                    hlssink.set_property_from_str("playlist-type", "vod");
                    hlssink.set_property("max-files", u32::MAX);
                }
                Some(MediaPlaylistType::Event) => {
                    hlssink.set_property_from_str("playlist-type", "event");
                }
                _ => {}
            }
            hlssink.set_property("i-frames-only", i_frames_only)
        } else {
            element_imp_error!(
                self,
                gst::ResourceError::Failed,
                ["hlssink3 is hard dependendency"]
            );
        }

        if let Some(parse) = h264parse {
            let hlssink_video_pad = s3hlssink
                .request_pad_simple("video")
                .expect("Failed to get hlssink video pad");
            let h264parse_srcpad = parse
                .static_pad("src")
                .expect("Failed to get h264parse source pad for linking");
            h264parse_srcpad
                .link(&hlssink_video_pad)
                .expect("Failed to link h264parse to hlssink");
        }

        if let Some(audio_tee) = audio_enc_tee {
            let queue_name = String::from("audio-queue-") + bitrate;
            let queue = gst::ElementFactory::make("queue")
                .name(&queue_name)
                .build()
                .expect("failed to create queue");
            self.setup_queue(&queue);

            self.obj().add(&queue).expect("failed to add queue to bin");
            audio_tee
                .link(&queue)
                .expect("Failed to link audiotee to queue");
            queue
                .link(&s3hlssink)
                .expect("Failed to link audiotee to hlssink");
        }
        // Keep s3hlssink to report stats
        settings.s3_hls_sinks.push(s3hlssink);
    }

    fn setup(&self) {
        let obj = self.obj();
        let mut settings = self.settings.lock().expect("Failed to get settings lock");

        //s3bucket, s3region do not have default value
        assert_ne!(settings.s3bucket, None, "s3Bucket cannot be None");
        assert_ne!(settings.s3region, None, "s3region cannot be None");

        gst::debug!(
            CAT,
            imp: self,
            "setup is_video_enable:{} is_audio_enable:{}",
            settings.video_sink,
            settings.audio_sink
        );

        let mut audio_enc_tee = None;
        // Set up Audio if enabled
        if settings.audio_sink {
            audio_enc_tee = gst::ElementFactory::make("tee")
                .name(AUDIO_ENCODER_OUT_TEE)
                .build()
                .ok();
            obj.add(
                audio_enc_tee
                    .as_ref()
                    .expect("audio tee should be available"),
            )
            .expect("Failed to add audio encoder output tee");

            self.create_audio_encoder_pipeline(
                audio_enc_tee
                    .as_ref()
                    .expect("audio tee should be available"),
                settings.audio_queue.as_ref().expect("audio queue not set"),
            );
        }

        // Setup video if enabled
        if settings.video_sink {
            let variants = settings.variants.clone();

            for variant in &variants {
                let h264parse = self.create_video_encoder_pipeline(
                    variant,
                    settings.target_duration,
                    settings
                        .video_in_tee
                        .as_ref()
                        .expect("video tee should be available"),
                );
                // dont mux audio for I-frame-only stream
                let audio_tee = if variant.iframe_only {
                    None
                } else {
                    audio_enc_tee.as_ref()
                };

                self.create_and_link_hlssink(
                    Some(h264parse),
                    audio_tee,
                    &mut settings,
                    &variant.bitrate.to_string(),
                    variant.iframe_only,
                );
            }
        } else {
            self.create_and_link_hlssink(
                None,
                audio_enc_tee.as_ref(),
                &mut settings,
                &DEFAULT_AUDIO_BITRATE.to_string(),
                false,
            );
        }

        let master_m3u8 =
            generate_master_m3u8(&settings.variants, &settings.prefix, settings.video_sink);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime")
            .block_on(upload_master_m3u8_to_s3(&settings, master_m3u8))
            .unwrap_or_else(|_e| {
                element_imp_error!(
                    self,
                    gst::ResourceError::Write,
                    ["Failed to upload master m3u8 to s3"]
                );
            });
    }
}

#[glib::object_subclass]
impl ObjectSubclass for EncS3HlsBin {
    const NAME: &'static str = "EncS3HlsBin";
    type Type = super::EncS3HlsBin;
    type ParentType = gst::Bin;

    fn with_class(_klass: &Self::Class) -> Self {
        Self {
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl BinImpl for EncS3HlsBin {}

impl ObjectImpl for EncS3HlsBin {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecUInt::builder("target-duration")
                    .nick("target duration of media segments")
                    .blurb("target duration of media segments")
                    .minimum(2)
                    .maximum(9)
                    .default_value(6)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("playlist-length")
                    .nick("number of segments to keep in playlist")
                    .blurb("number of segments to keep in playlist")
                    .minimum(3)
                    .maximum(10)
                    .default_value(5)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("playlist-type")
                    .nick("Playlist Type")
                    .blurb("The type of the playlist to use. When VOD type is set, the playlist will be live until the pipeline ends execution.")
                    .default_value(None)
                    .readwrite()
                    .build(),
                glib::ParamSpecString::builder("s3key-prefix")
                    .nick("s3key prefix to use for m3u8 and segments")
                    .blurb("s3key prefix to use for m3u8 and segments")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("s3bucket")
                    .nick("s3bucket to update the segments and playlist")
                    .blurb("s3bucket to update the segments and playlist")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("s3region")
                    .nick("s3region to update the segments and playlist")
                    .blurb("s3region to update the segments and playlist")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("access-key")
                    .nick("Access Key")
                    .blurb("AWS Access Key")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("secret-access-key")
                    .nick("Secret Access Key")
                    .blurb("AWS Secret Access Key")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("session-token")
                    .nick("session-token")
                    .blurb("AWS Session Token with assumeRole")
                    .default_value(None)
                    .mutable_ready()
                    .build(),
                gst::ParamSpecArray::builder("variants")
                    .nick("parameters for each output variant")
                    .blurb("parameters for each output variant")
                    .element_spec(
                        &glib::ParamSpecBoxed::builder::<gst::Structure>("variant-params")
                             .nick("structure with width,height,fps,bitrate,iframe-only")
                             .blurb("structure with width,height,fps,bitrate,iframe-only")
                             .mutable_ready()
                             .build(),
                        )
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Various statistics")
                    .blurb("Various statistics")
                    .read_only()
                    .build()
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().expect("Failed to get settings lock");

        gst::debug!(
            CAT,
            imp: self,
            "Setting property '{}' to '{:?}'",
            pspec.name(),
            value
        );

        match pspec.name() {
            "target-duration" => {
                let targe_duration = value
                    .get::<u32>()
                    .expect("target-duration typecheck failed");
                settings.target_duration = targe_duration;
            }
            "playlist-length" => {
                let playlist_len = value.get::<u32>().expect("playlist-len typecheck failed");
                settings.playlist_len = playlist_len;
            }
            "s3key-prefix" => {
                let s3prefix: String = value.get().expect("s3key-prefix typecheck failed");
                settings.prefix = s3prefix;
            }
            "playlist-type" => {
                let playlist_type = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .map(|chosen_type| {
                        if chosen_type.to_lowercase() == "vod" {
                            MediaPlaylistType::Vod
                        } else {
                            MediaPlaylistType::Event
                        }
                    });
                settings.playlist_type = playlist_type;
            }
            "s3bucket" => {
                let s3bucket: String = value.get().expect("s3Bucket typecheck failed");
                settings.s3bucket = Some(s3bucket);
            }
            "s3region" => {
                let s3region: String = value.get().expect("s3Region typecheck failed");
                settings.s3region = Some(s3region);
            }
            "access-key" => {
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "session-token" => {
                settings.session_token = value.get().expect("type checked upstream");
            }
            "variants" => {
                let objs = value
                    .get::<gst::ArrayRef>()
                    .expect("variants must be of Array type")
                    .iter()
                    .map(|v| {
                        let s = v
                            .get::<gst::Structure>()
                            .expect("variant props must have width,height,fps,bitrate,prefix");
                        VariantProps::from(s)
                    })
                    .collect::<Vec<_>>();
                settings.variants = objs;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().expect("Failed to get settings lock");

        match pspec.name() {
            "target-duration" => settings.target_duration.to_value(),
            "playlist-length" => settings.playlist_len.to_value(),
            "s3key-prefix" => settings.prefix.to_value(),
            "s3bucket" => settings.s3bucket.to_value(),
            "s3region" => settings.s3region.to_value(),
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "session-token" => settings.session_token.to_value(),
            "playlist-type" => settings
                .playlist_type
                .as_ref()
                .map(|ptype| ptype.to_string())
                .to_value(),
            "variants" => {
                let variants = settings
                    .variants
                    .iter()
                    .map(|x| gst::Structure::from(x.clone()).to_send_value())
                    .collect::<Vec<_>>();

                gst::Array::from_values(variants).to_value()
            }
            "stats" => self.create_stats(&settings).to_value(),
            _ => unimplemented!(),
        }
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: Lazy<Vec<glib::subclass::Signal>> = Lazy::new(|| {
            vec![glib::subclass::Signal::builder(SIGNAL_OVERRUN)
                .param_types([gst::Element::static_type()])
                .return_type::<()>()
                .build()]
        });

        SIGNALS.as_ref()
    }
}

impl GstObjectImpl for EncS3HlsBin {}

impl ElementImpl for EncS3HlsBin {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Multivariant HLS to S3 Bin",
                "Generic/Bin/Sink",
                "Convenience encoding/muxing/segmenting/sink element",
                "Daily. Co",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let audio_caps = gst::Caps::builder("audio/x-raw").build();

            let audio_sink_pad_template = gst::PadTemplate::new(
                HLS_AUDIO_PAD_NAME,
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &audio_caps,
            )
            .expect("s3hls-bin always has a audio pad template");
            let video_caps = gst::Caps::builder("video/x-raw").build();
            let video_sink_pad_template = gst::PadTemplate::new(
                HLS_VIDEO_PAD_NAME,
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &video_caps,
            )
            .expect("s3hls-bin always has a video pad template");
            vec![audio_sink_pad_template, video_sink_pad_template]
        });
        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        _name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let obj = self.obj();
        let mut settings = self.settings.lock().expect("Failed to get settings lock");

        match templ.name_template() {
            HLS_AUDIO_PAD_NAME => {
                if settings.audio_sink {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "requested_new_pad: {} pad is already set",
                        HLS_AUDIO_PAD_NAME
                    );
                    return None;
                }

                settings.audio_queue = gst::ElementFactory::make("queue")
                    .name(HLS_AUDIO_IN_QUEUE)
                    .build()
                    .ok();

                self.setup_queue(
                    settings
                        .audio_queue
                        .as_ref()
                        .expect("audio queue must be set"),
                );

                let audio_queue = settings
                    .audio_queue
                    .as_ref()
                    .expect("audio queue must be set");
                obj.add(audio_queue).expect("Failed to add audio queue");

                let peer_pad = audio_queue
                    .static_pad("sink")
                    .expect("audio queue should have a sink pad");
                let sink_pad = gst::GhostPad::from_template_with_target(
                    templ,
                    Some(HLS_AUDIO_PAD_NAME),
                    &peer_pad,
                )
                .expect("s3hls-bin always has a audio sink pad template");

                obj.add_pad(&sink_pad)
                    .expect("Failed to add audio sink pad");
                sink_pad
                    .set_active(true)
                    .expect("Failed to activate audio sink pad");
                settings.audio_sink = true;

                Some(sink_pad.upcast())
            }
            HLS_VIDEO_PAD_NAME => {
                if settings.video_sink {
                    gst::debug!(
                        CAT,
                        imp: self,
                        "requested_new_pad: {} pad is already set",
                        HLS_VIDEO_PAD_NAME
                    );
                    return None;
                }

                settings.video_in_tee = gst::ElementFactory::make("tee")
                    .name(HLS_VIDEO_IN_TEE)
                    .build()
                    .ok();

                let video_tee = settings
                    .video_in_tee
                    .as_ref()
                    .expect("video tee must be set");
                obj.add(video_tee).expect("Failed to add video queue");

                let peer_pad = video_tee
                    .static_pad("sink")
                    .expect("video tee should have a sink pad");
                let sink_pad = gst::GhostPad::from_template_with_target(
                    templ,
                    Some(HLS_VIDEO_PAD_NAME),
                    &peer_pad,
                )
                .expect("s3hls-bin always has a video sink pad template");

                obj.add_pad(&sink_pad).expect("Failed to video sink pad");
                sink_pad
                    .set_active(true)
                    .expect("Failed to activate video sink pad");
                settings.video_sink = true;

                Some(sink_pad.upcast())
            }
            _ => {
                gst::debug!(
                    CAT,
                    imp: self,
                    "requested_new_pad: is not {} or {}",
                    HLS_AUDIO_PAD_NAME,
                    HLS_VIDEO_PAD_NAME
                );
                None
            }
        }
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let obj = self.obj();

        let mut settings = self.settings.lock().expect("Failed to get settings lock");
        if !settings.audio_sink && !settings.video_sink {
            return;
        }

        let ghost_pad = pad
            .downcast_ref::<gst::GhostPad>()
            .expect("Pad to ghost pad downcast failed");
        pad.set_active(false).expect("Failed to deactivate pad");
        obj.remove_pad(pad).expect("Failed to remove pad");

        if HLS_AUDIO_PAD_NAME == ghost_pad.name() {
            settings.audio_sink = false;
        } else {
            settings.video_sink = false;
        }
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let gst::StateChange::ReadyToPaused = transition {
            self.setup();
        }

        self.parent_change_state(transition)
        // TODO: Do we need to delete the master.m3u8 and other m3u8 for
        // non-vod playlist ? some clients might be watching the live stream
        // so segments cannot be delete immediately. as per standard segments
        // should be delete after (playlist-len+1)*segment-duration.
    }
}
