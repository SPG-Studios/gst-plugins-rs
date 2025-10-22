// Copyright (C) 2022, Daily
//      Author: Arun Raghavan <arun@asymptotic.io>
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
// Copyright (C) 2024, Asymptotic Inc.
//      Author: Sanchayan Maity <sanchayan@asymptotic.io>
// Copyright (C) 2025, GlobalM SA
//      Author: Mart Raudsepp <mart.raudsepp@globalm.media>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::io::Write;
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::LazyLock;
use std::sync::Mutex;
use std::thread::{spawn, JoinHandle};
use std::time::Duration;

use gst::{element_imp_error, glib, prelude::*, subclass::prelude::*};

use aws_sdk_s3::{
    config::{self, retry::RetryConfig, Credentials, Region},
    primitives::ByteStream,
    types::ObjectCannedAcl,
    Client,
};
use aws_types::sdk_config::SdkConfig;

use crate::s3utils;

/*
 * We use a conservative channel size of 32. Using an unbounded channel or higher
 * channel size results in whole bunch of pending requests. For example, in case
 * of an unbounded channel by the time we finish uploading 10th request, 100+
 * requests might have already queued up.
 */
const S3_CHANNEL_SIZE: usize = 32;
const S3_ACL_DEFAULT: ObjectCannedAcl = ObjectCannedAcl::Private;
const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_TIMEOUT_IN_MSECS: u64 = 15000;
const DEFAULT_FORCE_PATH_STYLE: bool = false;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "awss3hlsmultivariantsink",
        gst::DebugColorFlags::empty(),
        Some("S3 HLS multivariant sink"),
    )
});

#[derive(Default)]
pub(crate) struct S3HlsMultivariantSinkPad {}

#[glib::object_subclass]
impl ObjectSubclass for S3HlsMultivariantSinkPad {
    const NAME: &'static str = "S3HlsMultivariantSinkPad";
    type Type = super::S3HlsMultivariantSinkPad;
    type ParentType = gst::GhostPad;
}

impl ObjectImpl for S3HlsMultivariantSinkPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoxed::builder::<gst::Structure>("alternate-rendition")
                    .nick("Rendition")
                    .blurb("Alternate Rendition")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("variant")
                    .nick("Variant")
                    .blurb("Variant Stream")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("playlist-location")
                    .nick("Playlist location")
                    .blurb("Location of the media playlist to write")
                    .build(),
                glib::ParamSpecString::builder("init-segment-location")
                    .nick("Init segment location")
                    .blurb("Location of the init segment file to write for CMAF")
                    .build(),
                glib::ParamSpecString::builder("segment-location")
                    .nick("Segment location")
                    .blurb("Location of the media segment file to write")
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if let Some(hlsmultivariantsink_pad) = self.obj().target() {
            hlsmultivariantsink_pad.set_property_from_value(pspec.name(), value);
        } else {
            unimplemented!();
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        if let Some(hlsmultivariantsink_pad) = self.obj().target() {
            hlsmultivariantsink_pad.property(pspec.name())
        } else {
            unimplemented!();
        }
    }
}

impl GstObjectImpl for S3HlsMultivariantSinkPad {}

impl PadImpl for S3HlsMultivariantSinkPad {}

impl ProxyPadImpl for S3HlsMultivariantSinkPad {}

impl GhostPadImpl for S3HlsMultivariantSinkPad {}

struct Settings {
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    s3_region: Region,
    s3_bucket: Option<String>,
    s3_key_prefix: Option<String>,
    s3_acl: ObjectCannedAcl,
    s3_upload_handle: Option<JoinHandle<()>>,
    s3_tx: Option<SyncSender<S3Request>>,
    s3_txc: Option<SyncSender<S3RequestControl>>,
    request_timeout: Duration,
    retry_attempts: u32,
    config: Option<SdkConfig>,
    endpoint_uri: Option<String>,
    force_path_style: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let duration = Duration::from_millis(DEFAULT_TIMEOUT_IN_MSECS);
        Self {
            access_key: None,
            secret_access_key: None,
            session_token: None,
            s3_region: Region::new("us-west-2"),
            s3_bucket: None,
            s3_key_prefix: None,
            s3_acl: S3_ACL_DEFAULT,
            s3_upload_handle: None,
            s3_tx: None,
            s3_txc: None,
            request_timeout: duration,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            config: None,
            endpoint_uri: None,
            force_path_style: DEFAULT_FORCE_PATH_STYLE,
        }
    }
}

pub struct S3HlsMultivariantSink {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    hlsmultivariantsink: gst::Element,
    canceller: Mutex<s3utils::Canceller>,
}

#[derive(Clone)]
struct S3Upload {
    s3_client: Client,
    s3_bucket: String,
    s3_key: String,
    s3_acl: ObjectCannedAcl,
    s3_tx: SyncSender<S3Request>,
    s3_data: Vec<u8>,
    last_flush_len: usize,
}

struct S3UploadReq {
    s3_client: Client,
    s3_bucket: String,
    s3_key: String,
    s3_acl: ObjectCannedAcl,
    s3_data: Vec<u8>,
}

struct S3DeleteReq {
    s3_client: Client,
    s3_bucket: String,
    s3_key: String,
}

enum S3Request {
    Upload(S3UploadReq),
    Delete(S3DeleteReq),
    Stop,
}

enum S3RequestControl {
    Continue,
    Pause,
}

enum State {
    Stopped,
    Started(Started),
}

#[derive(Default)]
struct Started {
    num_uploads_started: usize,
    num_uploads_completed: usize,
    num_bytes_uploaded: usize,
}

impl S3Upload {
    fn new(
        s3_client: Client,
        settings: &Settings,
        s3_location: String,
        s3_tx: SyncSender<S3Request>,
    ) -> S3Upload {
        let s3_bucket = settings.s3_bucket.as_ref().unwrap().to_string();
        let s3_key_prefix = settings.s3_key_prefix.as_ref();
        let s3_key = if let Some(key_prefix) = s3_key_prefix {
            format!("{key_prefix}/{s3_location}")
        } else {
            s3_location
        };
        let s3_acl = settings.s3_acl.clone();

        S3Upload {
            s3_client,
            s3_bucket,
            s3_key,
            s3_acl,
            s3_data: Vec::new(),
            s3_tx,
            last_flush_len: 0,
        }
    }

    fn flush(&mut self, consume: bool) {
        let s3_data_len = self.s3_data.len();
        if self.last_flush_len == s3_data_len {
            // Nothing new was written, nothing to flush
            gst::debug!(
                CAT,
                "Buffer length ({}) unchanged, skipping flush for {}",
                s3_data_len,
                self.s3_key
            );
            return;
        }

        let s3_data: Vec<u8> = if consume {
            self.s3_data.drain(0..).collect()
        } else {
            self.s3_data.clone()
        };
        let s3_tx = &mut self.s3_tx;
        let s3_channel = S3UploadReq {
            s3_client: self.s3_client.clone(),
            s3_bucket: self.s3_bucket.clone(),
            s3_key: self.s3_key.clone(),
            s3_acl: self.s3_acl.clone(),
            s3_data,
        };

        gst::debug!(
            CAT,
            "Submitting {}consuming upload request for key: {}",
            if consume { "" } else { "non-" },
            s3_channel.s3_key,
        );

        match s3_tx.send(S3Request::Upload(s3_channel)) {
            Ok(()) => {
                self.last_flush_len = s3_data_len;
                gst::debug!(
                    CAT,
                    "Send S3 key {} of data length {} succeeded",
                    self.s3_key,
                    s3_data_len,
                );
            }
            Err(_) => {
                /*
                 * A send operation can only fail if the receiving end of a
                 * channel is disconnected, implying that the data would
                 * never be received.
                 */
                gst::error!(
                    CAT,
                    "Send S3 key {} of data length {} failed",
                    self.s3_key,
                    s3_data_len,
                );
            }
        }
    }
}

impl Write for S3Upload {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        gst::log!(CAT, "Write {}, {}", self.s3_key, buf.len());
        self.s3_data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush(false);
        Ok(())
    }
}

impl Drop for S3Upload {
    fn drop(&mut self) {
        self.flush(true);
    }
}

impl S3HlsMultivariantSink {
    fn s3_get_stream(
        &self,
        args: &[glib::Value],
        tx: SyncSender<S3Request>,
    ) -> Option<glib::Value> {
        let s3client = self.s3client_from_settings();
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        match *state {
            State::Started(ref mut state) => state.num_uploads_started += 1,
            State::Stopped => unreachable!("State not started yet"),
        };
        drop(state);

        let s3_location = args[1].get::<&str>().unwrap();
        let upload = S3Upload::new(s3client, &settings, s3_location.to_string(), tx);

        gst::debug!(CAT, imp = self, "New upload for {}", s3_location);

        Some(
            gio::WriteOutputStream::new(upload)
                .upcast::<gio::OutputStream>()
                .to_value(),
        )
    }

    fn s3_request(&self, rxc: Receiver<S3RequestControl>, rx: Receiver<S3Request>) {
        loop {
            match rxc.try_recv() {
                Ok(S3RequestControl::Continue) => (),
                Ok(S3RequestControl::Pause) => {
                    gst::debug!(CAT, imp = self, "Pausing S3 request thread.");
                    match rxc.recv() {
                        Ok(S3RequestControl::Continue) => {
                            gst::debug!(CAT, imp = self, "Continuing S3 request thread.")
                        }
                        // We do not expect another pause request here.
                        Ok(S3RequestControl::Pause) => unreachable!(),
                        Err(_) => (),
                    }
                }
                /*
                 * We are not concerned with `Empty` and since we close the control
                 * channel ourselves when required, `Disconnected` will be expected.
                 */
                Err(_) => (),
            };

            match rx.recv() {
                Ok(S3Request::Upload(data)) => {
                    let s3_client = data.s3_client.clone();
                    let s3_bucket = data.s3_bucket.clone();
                    let s3_key = data.s3_key.clone();
                    let s3_acl = data.s3_acl;
                    let s3_data_len = data.s3_data.len();

                    gst::debug!(CAT, imp = self, "Uploading key {}", s3_key);

                    let put_object_req = s3_client
                        .put_object()
                        .set_bucket(Some(s3_bucket))
                        .set_key(Some(s3_key.clone()))
                        .set_body(Some(ByteStream::from(data.s3_data)))
                        .set_acl(Some(s3_acl));
                    let put_object_req_future = put_object_req.send();
                    let result = s3utils::wait(&self.canceller, put_object_req_future);

                    match result {
                        Err(err) => {
                            gst::error!(
                                CAT,
                                imp = self,
                                "Put object request for S3 key {} of data length {} failed with error {err}",
                                s3_key,
                                s3_data_len,
                            );
                            element_imp_error!(
                                self,
                                gst::ResourceError::Write,
                                ["Put object request failed"]
                            );
                            break;
                        }
                        Ok(_) => {
                            let mut state = self.state.lock().unwrap();
                            match *state {
                                State::Started(ref mut state) => {
                                    state.num_bytes_uploaded += s3_data_len;
                                    state.num_uploads_completed += 1;
                                }
                                State::Stopped => {
                                    unreachable!("State not started yet")
                                }
                            };
                        }
                    };
                }
                Ok(S3Request::Delete(data)) => {
                    let s3_client = data.s3_client.clone();
                    let s3_bucket = data.s3_bucket.clone();
                    let s3_key = data.s3_key.clone();

                    gst::debug!(CAT, imp = self, "Deleting key {}", s3_key);

                    let delete_object_req = s3_client
                        .delete_object()
                        .set_bucket(Some(s3_bucket))
                        .set_key(Some(s3_key.clone()));
                    let delete_object_req_future = delete_object_req.send();
                    let result = s3utils::wait(&self.canceller, delete_object_req_future);

                    if let Err(err) = result {
                        gst::error!(
                            CAT,
                            imp = self,
                            "Delete object request for S3 key {} failed with error {err}",
                            s3_key,
                        );
                        element_imp_error!(
                            self,
                            gst::ResourceError::Write,
                            ["Delete object request failed"]
                        );
                        break;
                    };
                }
                Ok(S3Request::Stop) => break,
                Err(err) => {
                    gst::error!(CAT, imp = self, "S3 channel error: {}", err);
                    element_imp_error!(self, gst::ResourceError::Write, ["S3 channel error"]);
                    break;
                }
            }
        }

        gst::info!(CAT, imp = self, "Exiting S3 request thread",);
    }

    fn s3client_from_settings(&self) -> Client {
        let mut settings = self.settings.lock().unwrap();

        if settings.config.is_none() {
            let timeout_config = s3utils::timeout_config(settings.request_timeout);
            let access_key = settings.access_key.as_ref();
            let secret_access_key = settings.secret_access_key.as_ref();
            let session_token = settings.session_token.clone();

            let cred = match (access_key, secret_access_key) {
                (Some(access), Some(secret_access)) => Some(Credentials::new(
                    access,
                    secret_access,
                    session_token,
                    None,
                    "s3-hlssink",
                )),
                _ => None,
            };

            let sdk_config = s3utils::wait_config(
                &self.canceller,
                settings.s3_region.clone(),
                timeout_config,
                cred,
            )
            .expect("Failed to get SDK config");

            settings.config = Some(sdk_config);
        }

        let sdk_config = settings.config.as_ref().expect("SDK config must be set");

        let config_builder = config::Builder::from(sdk_config)
            .force_path_style(settings.force_path_style)
            .region(settings.s3_region.clone())
            .retry_config(RetryConfig::standard().with_max_attempts(settings.retry_attempts));

        let config = if let Some(ref uri) = settings.endpoint_uri {
            config_builder.endpoint_url(uri).build()
        } else {
            config_builder.build()
        };

        Client::from_conf(config)
    }

    fn stop(&self) {
        let mut settings = self.settings.lock().unwrap();
        let s3_handle = settings.s3_upload_handle.take();
        let s3_tx = settings.s3_tx.clone();

        if let (Some(handle), Some(tx)) = (s3_handle, s3_tx) {
            gst::info!(CAT, imp = self, "Stopping S3 request thread");
            match tx.send(S3Request::Stop) {
                Ok(_) => {
                    gst::info!(CAT, imp = self, "Joining S3 request thread");
                    if let Err(err) = handle.join() {
                        gst::error!(
                            CAT,
                            imp = self,
                            "S3 upload thread failed to exit: {:?}",
                            err
                        );
                    }
                    drop(tx);
                }
                Err(err) => {
                    gst::error!(CAT, imp = self, "Failed to stop S3 request thread: {}", err)
                }
            };
        };

        let mut state = self.state.lock().unwrap();
        *state = State::Stopped;

        let mut canceller = self.canceller.lock().unwrap();
        canceller.abort();
        *canceller = s3utils::Canceller::None;
    }

    fn create_stats(&self) -> gst::Structure {
        let state = self.state.lock().unwrap();

        match &*state {
            State::Started(state) => gst::Structure::builder("stats")
                .field("num-uploads-started", state.num_uploads_started as u32)
                .field("num-uploads-completed", state.num_uploads_completed as u32)
                .field("num-bytes-uploaded", state.num_bytes_uploaded as u32)
                .build(),

            State::Stopped => gst::Structure::builder("stats")
                .field("num-uploads-started", 0)
                .field("num-uploads-completed", 0)
                .field("num-bytes-uploaded", 0)
                .build(),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3HlsMultivariantSink {
    const NAME: &'static str = "GstAwsS3HlsMultivariantSink";
    type Type = super::S3HlsMultivariantSink;
    type ParentType = gst::Bin;
    type Interfaces = (gst::ChildProxy,);

    fn with_class(_klass: &Self::Class) -> Self {
        let hlsmultivariantsink = gst::ElementFactory::make("hlsmultivariantsink")
            .name("hlsmultivariantsink")
            .build()
            .expect("Could not find hlsmultivariantsink element");

        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::Stopped),
            hlsmultivariantsink,
            canceller: Mutex::new(s3utils::Canceller::default()),
        }
    }
}

impl ObjectImpl for S3HlsMultivariantSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
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
                    .blurb("AWS temporary session token from STS")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("bucket")
                    .nick("S3 Bucket")
                    .blurb("The bucket of the file to write")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("key-prefix")
                    .nick("S3 key prefix")
                    .blurb("The key prefix for segment and playlist files")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("region")
                    .nick("AWS Region")
                    .blurb("The AWS region for the S3 bucket (e.g. eu-west-2).")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecObject::builder::<gst::Element>("hlsmultivariantsink")
                    .nick("HLS Multivariant Sink")
                    .blurb("The underlying HLS multivariant sink being used")
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("acl")
                    .nick("S3 ACL")
                    .blurb("Canned ACL to use for uploading to S3")
                    .default_value(Some(S3_ACL_DEFAULT.as_str()))
                    .build(),
                glib::ParamSpecUInt::builder("retry-attempts")
                    .nick("Retry attempts")
                    .blurb(
                        "Number of times AWS SDK attempts a request before abandoning the request",
                    )
                    .minimum(1)
                    .maximum(10)
                    .default_value(DEFAULT_RETRY_ATTEMPTS)
                    .build(),
                glib::ParamSpecUInt64::builder("request-timeout")
                    .nick("API call timeout")
                    .blurb("Timeout for request to S3 service (in ms)")
                    .minimum(1)
                    .default_value(DEFAULT_TIMEOUT_IN_MSECS)
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("stats")
                    .nick("Various statistics")
                    .blurb("Various statistics")
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("endpoint-uri")
                    .nick("S3 endpoint URI")
                    .blurb("The S3 endpoint URI to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("force-path-style")
                    .nick("Force path style")
                    .blurb("Force client to use path-style addressing for buckets")
                    .default_value(DEFAULT_FORCE_PATH_STYLE)
                    .build(),
            ]
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
            "access-key" => {
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "session-token" => {
                settings.session_token = value.get().expect("type checked upstream");
            }
            "bucket" => {
                settings.s3_bucket = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "key-prefix" => {
                settings.s3_key_prefix = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "region" => {
                let region = value.get::<String>().expect("type checked upstream");
                settings.s3_region = Region::new(region);
            }
            "acl" => {
                let s3_acl = value.get::<String>().expect("type checked upstream");
                settings.s3_acl = ObjectCannedAcl::from_str(&s3_acl).expect("Invalid ACL");
            }
            "retry-attempts" => {
                settings.retry_attempts = value.get::<u32>().expect("type checked upstream");
            }
            "request-timeout" => {
                settings.request_timeout =
                    Duration::from_millis(value.get::<u64>().expect("type checked upstream"));
            }
            "endpoint-uri" => {
                settings.endpoint_uri = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
            }
            "force-path-style" => {
                settings.force_path_style = value.get::<bool>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "session-token" => settings.session_token.to_value(),
            "key-prefix" => settings.s3_key_prefix.to_value(),
            "bucket" => settings.s3_bucket.to_value(),
            "region" => settings.s3_region.to_string().to_value(),
            "hlsmultivariantsink" => self.hlsmultivariantsink.to_value(),
            "acl" => settings.s3_acl.as_str().to_value(),
            "retry-attempts" => settings.retry_attempts.to_value(),
            "request-timeout" => (settings.request_timeout.as_millis() as u64).to_value(),
            "stats" => self.create_stats().to_value(),
            "endpoint-uri" => settings.endpoint_uri.to_value(),
            "force-path-style" => settings.force_path_style.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        // TODO: Why don't we get this automatically via hlsmultivariantsink child?
        obj.set_element_flags(gst::ElementFlags::SINK);

        obj.add(&self.hlsmultivariantsink).unwrap();

        let mut settings = self.settings.lock().unwrap();

        let (txc, rxc): (SyncSender<S3RequestControl>, Receiver<S3RequestControl>) =
            mpsc::sync_channel(S3_CHANNEL_SIZE);
        let (tx, rx): (SyncSender<S3Request>, Receiver<S3Request>) =
            mpsc::sync_channel(S3_CHANNEL_SIZE);

        let s3_tx = tx.clone();
        let multivariant_playlist_tx = tx.clone();
        let playlist_tx = tx.clone();
        let fragment_tx = tx.clone();
        let init_tx = tx.clone();
        let delete_tx = tx;

        let self_ = self.ref_counted();
        let handle = spawn(move || self_.s3_request(rxc, rx));

        settings.s3_upload_handle = Some(handle);
        settings.s3_tx = Some(s3_tx);
        settings.s3_txc = Some(txc);
        drop(settings);

        gst::info!(CAT, imp = self, "Constructed");

        self.hlsmultivariantsink
            .connect("get-multivariant-playlist-stream", false, {
                let self_weak = self.downgrade();
                move |args| -> Option<glib::Value> {
                    let self_ = self_weak.upgrade()?;
                    self_.s3_get_stream(args, multivariant_playlist_tx.clone())
                }
            });

        self.hlsmultivariantsink
            .connect("get-playlist-stream", false, {
                let self_weak = self.downgrade();
                move |args| -> Option<glib::Value> {
                    let self_ = self_weak.upgrade()?;
                    self_.s3_get_stream(args, playlist_tx.clone())
                }
            });

        self.hlsmultivariantsink
            .connect("get-fragment-stream", false, {
                let self_weak = self.downgrade();
                move |args| -> Option<glib::Value> {
                    let self_ = self_weak.upgrade()?;
                    self_.s3_get_stream(args, fragment_tx.clone())
                }
            });

        self.hlsmultivariantsink.connect("get-init-stream", false, {
            let self_weak = self.downgrade();
            move |args| -> Option<glib::Value> {
                let self_ = self_weak.upgrade()?;
                self_.s3_get_stream(args, init_tx.clone())
            }
        });

        self.hlsmultivariantsink.connect("delete-fragment", false, {
            let self_weak = self.downgrade();
            move |args| -> Option<glib::Value> {
                let self_ = self_weak.upgrade()?;
                let s3_client = self_.s3client_from_settings();
                let settings = self_.settings.lock().unwrap();

                let s3_bucket = settings.s3_bucket.as_ref().unwrap().clone();
                let s3_location = args[1].get::<String>().unwrap();

                let s3_key_prefix = settings.s3_key_prefix.as_ref();
                let s3_key = if let Some(key_prefix) = s3_key_prefix {
                    format!("{key_prefix}/{s3_location}")
                } else {
                    s3_location.to_string()
                };

                gst::debug!(CAT, imp = self_, "Deleting {}", s3_location);

                let delete = S3DeleteReq {
                    s3_client,
                    s3_bucket,
                    s3_key,
                };

                let res = delete_tx.send(S3Request::Delete(delete));

                // The signature on delete-fragment signal is different for
                // hlssink2 and hlssink3.
                if self_
                    .hlsmultivariantsink
                    .factory()
                    .is_some_and(|factory| factory.name() == "hlssink3")
                {
                    if res.is_ok() {
                        Some(true.to_value())
                    } else {
                        gst::error!(CAT, imp = self_, "Failed deleting {}", s3_location);
                        element_imp_error!(
                            self_,
                            gst::ResourceError::Write,
                            ["Failed to delete fragment"]
                        );
                        Some(false.to_value())
                    }
                } else {
                    None
                }
            }
        });
    }
}

impl GstObjectImpl for S3HlsMultivariantSink {}

impl ElementImpl for S3HlsMultivariantSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "S3 HLS Multivariant Sink",
                "Sink/Muxer",
                "Streams multivariant HLS data to S3",
                "GlobalM SA",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps_any = gst::Caps::new_any();

            let audio_pad_template = gst::PadTemplate::with_gtype(
                "audio_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps_any,
                super::S3HlsMultivariantSinkPad::static_type(),
            )
            .unwrap();

            let video_pad_template = gst::PadTemplate::with_gtype(
                "video_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &caps_any,
                super::S3HlsMultivariantSinkPad::static_type(),
            )
            .unwrap();

            vec![audio_pad_template, video_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let ret = self.parent_change_state(transition)?;
        /*
         * The settings lock must not be taken before the parent state change.
         * Parent state change will result in the callback getting called which
         * in turn will require the settings lock.
         */
        let settings = self.settings.lock().unwrap();

        /*
         * We do not call abort on the canceller in change_state here as
         * that results in the final playlist and media segment uploads
         * being aborted leaving the media segments and playlist in an
         * unplayable state. All finalisation is carried out in `stop`
         * which is called for ReadyToNull transition.
         */
        match transition {
            gst::StateChange::ReadyToPaused => {
                let mut state = self.state.lock().unwrap();
                *state = State::Started(Started::default());
            }
            gst::StateChange::PausedToPlaying => {
                let s3_txc = settings.s3_txc.clone();
                if let Some(tx) = s3_txc {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Sending continue request to S3 request thread."
                    );
                    if tx.send(S3RequestControl::Continue).is_err() {
                        gst::error!(CAT, imp = self, "Could not send continue request.");
                    }
                }
            }

            gst::StateChange::PlayingToPaused => {
                let s3_txc = settings.s3_txc.clone();
                if let Some(tx) = s3_txc {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Sending pause request to S3 request thread."
                    );
                    if settings.s3_upload_handle.is_some()
                        && tx.send(S3RequestControl::Pause).is_err()
                    {
                        gst::error!(CAT, imp = self, "Could not send pause request.");
                    }
                }
            }

            gst::StateChange::ReadyToNull => {
                drop(settings);
                /*
                 * Ready to Null transition will block till we finish uploading
                 * pending requests.
                 */
                self.stop();
            }
            _ => (),
        }

        Ok(ret)
    }

    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        gst::warning!(
            CAT,
            imp = self,
            "request_new_pad: templ \"{}\"; hlsmultivariantsink static pad templates: {:?}",
            templ.name(),
            self.hlsmultivariantsink
                .factory()
                .unwrap()
                .static_pad_templates()
                .iter()
                .map(|p| p.name_template())
                .collect::<Vec<_>>()
        );

        let child_pad_templ = self.hlsmultivariantsink.pad_template(&templ.name())?;
        let pad = self
            .hlsmultivariantsink
            .request_pad(&child_pad_templ, name, caps)?;
        let sink_pad = gst::PadBuilder::<super::S3HlsMultivariantSinkPad>::from_template(templ)
            .name(pad.name())
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();
        sink_pad.set_target(Some(&pad)).unwrap();

        self.obj().add_pad(&sink_pad).unwrap();
        sink_pad.set_active(true).unwrap();
        self.obj()
            .child_added(sink_pad.upcast_ref::<gst::Object>(), &sink_pad.name());
        Some(sink_pad.upcast())
    }
}

impl BinImpl for S3HlsMultivariantSink {
    fn handle_message(&self, message: gst::Message) {
        use gst::MessageView;
        match message.view() {
            MessageView::Eos(_) | MessageView::Error(_) => {
                let mut settings = self.settings.lock().unwrap();
                let s3_txc = settings.s3_txc.take();
                if let Some(txc) = s3_txc {
                    /*
                     * A pause request would have been send to S3 request in PlayingToPause
                     * transition before ReadyToNull transition. Drop control channel here
                     * since we do not care about play pause transitions after EOS and to
                     * unblock the S3 request thread from waiting for a Continue request
                     * on the control channel.
                     */
                    gst::debug!(CAT, imp = self, "Got EOS, dropping control channel");
                    drop(txc);
                }
                drop(settings);
                self.parent_handle_message(message)
            }
            _ => self.parent_handle_message(message),
        }
    }
}

impl ChildProxyImpl for S3HlsMultivariantSink {
    fn child_by_name(&self, name: &str) -> Option<glib::Object> {
        if let Some(child) = self.parent_child_by_name(name) {
            Some(child)
        } else {
            self.obj()
                .pads()
                .into_iter()
                .find(|p| p.name() == name)
                .map(|p| p.upcast())
        }
    }
    fn child_by_index(&self, index: u32) -> Option<glib::Object> {
        let parent_children_count = self.parent_children_count();

        if index < parent_children_count {
            self.parent_child_by_index(index)
        } else {
            self.obj()
                .pads()
                .into_iter()
                .nth((index - parent_children_count) as usize)
                .map(|p| p.upcast())
        }
    }

    fn children_count(&self) -> u32 {
        let object = self.obj();
        self.parent_children_count() + object.num_pads() as u32
    }
}
