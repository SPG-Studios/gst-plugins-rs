// SPDX-License-Identifier: MPL-2.0

use crate::signaller::{Signallable, SignallableExt, SignallableImpl};

use crate::utils::{wait_async, WaitError};
use crate::RUNTIME;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_webrtc::WebRTCICEGatheringState;
use once_cell::sync::Lazy;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::StatusCode;
use serde::ser::{Error, SerializeMap};
use serde::{Deserialize, Serialize, Serializer};
use std::sync::Mutex;
use tokio::sync::oneshot;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "webrtc-cloudflare-calls-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC Cloudflare Calls signaller"),
    )
});

const DEFAULT_TIMEOUT: u32 = 15;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum SdpType {
    Offer,
    Answer,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDescription {
    sdp: String,
    #[serde(rename = "type")]
    ty: SdpType,
}

impl From<gst_webrtc::WebRTCSessionDescription> for SessionDescription {
    fn from(value: gst_webrtc::WebRTCSessionDescription) -> Self {
        Self {
            sdp: value.sdp().as_text().unwrap(),
            ty: match value.type_() {
                gst_webrtc::WebRTCSDPType::Offer => SdpType::Offer,
                gst_webrtc::WebRTCSDPType::Answer => SdpType::Answer,
                _ => unreachable!(),
            },
        }
    }
}

impl TryFrom<SessionDescription> for gst_webrtc::WebRTCSessionDescription {
    type Error = glib::BoolError;

    fn try_from(value: SessionDescription) -> Result<Self, Self::Error> {
        let sdp = gst_sdp::SDPMessage::parse_buffer(value.sdp.as_bytes())?;
        let ty = match value.ty {
            SdpType::Offer => gst_webrtc::WebRTCSDPType::Offer,
            SdpType::Answer => gst_webrtc::WebRTCSDPType::Answer,
        };
        Ok(gst_webrtc::WebRTCSessionDescription::new(ty, sdp))
    }
}

fn serialize_extra_fields<S>(extra_fields: &Option<gst::Structure>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if let Some(fields) = extra_fields {
        let mut map = s.serialize_map(Some(fields.len()))?;
        for (name, val) in fields.iter() {
            match val.type_() {
                glib::types::Type::I8 => {
                    map.serialize_entry(&name.as_str(), &val.get::<i8>().unwrap())?
                }
                glib::types::Type::U8 => {
                    map.serialize_entry(&name.as_str(), &val.get::<u8>().unwrap())?
                }
                glib::types::Type::I32 => {
                    map.serialize_entry(&name.as_str(), &val.get::<i32>().unwrap())?
                }
                glib::types::Type::U32 => {
                    map.serialize_entry(&name.as_str(), &val.get::<u32>().unwrap())?
                }
                glib::types::Type::I64 => {
                    map.serialize_entry(&name.as_str(), &val.get::<i64>().unwrap())?
                }
                glib::types::Type::U64 => {
                    map.serialize_entry(&name.as_str(), &val.get::<u64>().unwrap())?
                }
                glib::types::Type::F32 => {
                    map.serialize_entry(&name.as_str(), &val.get::<f32>().unwrap())?
                }
                glib::types::Type::F64 => {
                    map.serialize_entry(&name.as_str(), &val.get::<f64>().unwrap())?
                }
                glib::types::Type::STRING => {
                    map.serialize_entry(&name.as_str(), &val.get::<&str>().unwrap())?
                }
                ty => {
                    return Err(S::Error::custom(format!(
                        "Unable to serialize gst::Structure field \'{name}\' of type {ty}"
                    )))
                }
            }
        }
        map.end()
    } else {
        s.serialize_none()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionRequest {
    session_description: SessionDescription,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "serialize_extra_fields")]
    extra_fields: Option<gst::Structure>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionResponse {
    session_description: SessionDescription,
    session_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "location")]
#[serde(rename_all = "camelCase")]
enum TrackType {
    Local(LocalTrack),
    Remote(RemoteTrack),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteTrack {
    session_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalTrack {
    mid: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Track {
    track_name: String,
    #[serde(flatten, default)]
    ty: Option<TrackType>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TracksRequest {
    session_description: SessionDescription,
    tracks: Vec<Track>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "serialize_extra_fields")]
    extra_fields: Option<gst::Structure>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TrackOrError {
    Track(Track),
    Error(TrackError),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackError {
    error_code: String,
    error_description: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TracksResponse {
    requires_immediate_renegotiation: bool,
    session_description: SessionDescription,
    tracks: Vec<TrackOrError>,
}

#[derive(Clone)]
struct Settings {
    server_url: Option<String>,
    app_id: Option<String>,
    auth_token: Option<String>,
    session_id: Option<String>,
    timeout: u32,
    tracks: Vec<String>,
    extra_fields: Option<gst::Structure>,
}

fn url_path_with_app_id(path: &str, app_id: &Option<String>) -> String {
    if let Some(app_id) = app_id {
        format!("/apps/{app_id}/{path}")
    } else {
        path.to_string()
    }
}

fn url_path_with_app_id_session(path: &str, app_id: &Option<String>, session_id: &str) -> String {
    if path.is_empty() {
        url_path_with_app_id(&format!("sessions/{session_id}"), app_id)
    } else {
        url_path_with_app_id(&format!("sessions/{session_id}/{path}"), app_id)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: Some("https://rtc.live.cloudflare.com/v1".to_string()),
            app_id: None,
            auth_token: None,
            session_id: None,
            timeout: DEFAULT_TIMEOUT,
            tracks: vec![],
            extra_fields: None,
        }
    }
}

#[derive(Default)]
pub struct CallsClient {
    settings: Mutex<Settings>,
    canceller: Mutex<Option<futures::future::AbortHandle>>,
    client: Mutex<Option<reqwest::Client>>,
}

impl CallsClient {
    fn raise_error(&self, msg: String) {
        self.obj()
            .emit_by_name::<()>("error", &[&format!("Error: {msg}")]);
    }

    fn handle_future_error(&self, err: WaitError) {
        match err {
            WaitError::FutureAborted => {
                gst::warning!(CAT, imp = self, "Future aborted")
            }
            WaitError::FutureError(err) => self.raise_error(err.to_string()),
        };
    }

    async fn producer_send_offer(&self, webrtcbin: &gst::Element) {
        let local_desc =
            webrtcbin.property::<Option<gst_webrtc::WebRTCSessionDescription>>("local-description");

        let offer_sdp = match local_desc {
            None => {
                self.raise_error("Local description is not set".to_string());
                return;
            }
            Some(offer) => self.obj().munge_sdp("unique", &offer),
        };

        gst::debug!(
            CAT,
            imp = self,
            "Sending offer SDP: {:?}",
            offer_sdp.sdp().as_text()
        );

        let timeout;
        {
            let settings = self.settings.lock().unwrap();
            timeout = settings.timeout;
            drop(settings);
        }

        if let Err(e) = wait_async(
            &self.canceller,
            self.do_post_new_session(offer_sdp),
            timeout,
        )
        .await
        {
            self.handle_future_error(e);
        }
    }

    async fn do_post_new_session(&self, offer: gst_webrtc::WebRTCSessionDescription) {
        let auth_token;
        let endpoint;
        let app_id;
        let extra_fields;

        {
            let settings = self.settings.lock().unwrap();
            auth_token = settings.auth_token.clone();
            endpoint = reqwest::Url::parse(settings.server_url.as_ref().unwrap().as_str()).unwrap();
            app_id = settings.app_id.clone();
            extra_fields = settings.extra_fields.clone();
            drop(settings);
        }

        let Ok(endpoint) = endpoint.join(&url_path_with_app_id("sessions/new", &app_id)) else {
            self.raise_error("Failed to construct Url".to_string());
            return;
        };

        let client = reqwest::Client::builder().build().unwrap();

        let sess_request = NewSessionRequest {
            session_description: SessionDescription {
                sdp: offer.sdp().as_text().unwrap(),
                ty: SdpType::Offer,
            },
            extra_fields,
        };
        let request_str = serde_json::to_string(&sess_request).unwrap();
        gst::trace!(CAT, imp = self, "New session request JSON: {request_str}");

        let mut headermap = HeaderMap::new();

        if let Some(token) = auth_token.as_ref() {
            let bearer_token = "Bearer ".to_owned() + token;
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(bearer_token.as_str())
                    .expect("Failed to set auth token to header"),
            );
        }

        let res = client
            .request(reqwest::Method::POST, endpoint.clone())
            .headers(headermap)
            .json(&sess_request)
            .send()
            .await;

        match res {
            Ok(resp) => self.parse_new_session_response(resp, client).await,
            Err(err) => self.raise_error(err.to_string()),
        }
    }

    async fn parse_new_session_response(
        &self,
        response: reqwest::Response,
        client: reqwest::Client,
    ) {
        gst::debug!(CAT, imp = self, "response status: {}", response.status());

        match response.status() {
            StatusCode::OK | StatusCode::CREATED => {
                let answer = match response.json::<NewSessionResponse>().await {
                    Ok(answer) => answer,
                    Err(e) => {
                        self.raise_error(e.to_string());
                        return;
                    }
                };
                {
                    let mut inner = self.settings.lock().unwrap();
                    inner.session_id = Some(answer.session_id.clone());
                }
                let answer_desc: gst_webrtc::WebRTCSessionDescription =
                    match answer.session_description.try_into() {
                        Ok(answer) => answer,
                        Err(e) => {
                            self.raise_error(e.to_string());
                            return;
                        }
                    };
                self.obj()
                    .emit_by_name::<()>("session-description", &[&"unique", &answer_desc]);
                gst::info!(CAT, imp = self, "session: {}", answer.session_id);

                self.obj().notify("session-id");
                *self.client.lock().unwrap() = Some(client);
            }

            s => {
                match response.bytes().await {
                    Ok(r) => {
                        let res = r.escape_ascii().to_string();

                        // FIXME: Check and handle 'Retry-After' header in case of server error
                        self.raise_error(format!("Unexpected response: {} - {}", s.as_str(), res));
                    }
                    Err(err) => self.raise_error(err.to_string()),
                }
            }
        }
    }

    async fn producer_post_tracks(&self, webrtcbin: &gst::Element) {
        let auth_token;
        let endpoint;
        let app_id;
        let session_id;
        let extra_fields;
        let Some(client) = self.client.lock().unwrap().take() else {
            gst::trace!(CAT, imp = self, "post tracks already done");
            return;
        };

        {
            let settings = self.settings.lock().unwrap();
            auth_token = settings.auth_token.clone();
            endpoint = reqwest::Url::parse(settings.server_url.as_ref().unwrap().as_str()).unwrap();
            app_id = settings.app_id.clone();
            session_id = settings.session_id.clone().unwrap();
            extra_fields = settings.extra_fields.clone();
            drop(settings);
        }

        let (sender, receiver) = oneshot::channel();
        let webrtcbin_clone = webrtcbin.clone();
        let obj_clone = self.obj().clone();
        let promise = gst::Promise::with_change_func(move |result| {
            let structure = result.unwrap().unwrap();
            let offer = structure
                .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                .unwrap();
            let offer = obj_clone.munge_sdp("unique", &offer);
            let offer_clone = offer.clone();
            let promise = gst::Promise::with_change_func(move |_result| {
                sender.send(offer_clone).unwrap();
            });
            webrtcbin_clone.emit_by_name::<()>("set-local-description", &[&offer, &promise]);
        });
        webrtcbin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        let offer = match receiver.await {
            Ok(offer) => offer,
            Err(e) => {
                self.raise_error(format!("Failed to create-offer: {e:?}"));
                return;
            }
        };
        let mut error = false;
        let tracks = offer
            .sdp()
            .medias()
            .map(|media| {
                let mid;
                if let Some(mid_val) = media.attribute_val("mid") {
                    mid = mid_val;
                } else {
                    self.raise_error("media has no MID from webrtcbin!".to_string());
                    error = true;
                    mid = "ERROR";
                }
                Track {
                    track_name: rand::random::<u64>().to_string(),
                    ty: Some(TrackType::Local(LocalTrack {
                        mid: mid.to_string(),
                    })),
                }
            })
            .collect::<Vec<_>>();
        if error {
            return;
        }

        let Ok(endpoint) = endpoint.join(&url_path_with_app_id_session(
            "tracks/new",
            &app_id,
            &session_id,
        )) else {
            self.raise_error("Failed to construct Url".to_string());
            return;
        };

        let mut headermap = HeaderMap::new();
        if let Some(token) = auth_token.as_ref() {
            let bearer_token = "Bearer ".to_owned() + token;
            headermap.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(bearer_token.as_str())
                    .expect("Failed to set auth token to header"),
            );
        }

        let tracks_request = TracksRequest {
            session_description: offer.clone().into(),
            tracks,
            extra_fields,
        };
        let json = serde_json::to_string(&tracks_request).unwrap();
        gst::trace!(CAT, imp = self, "new tracks request json {json}");

        let res = client
            .request(reqwest::Method::POST, endpoint.clone())
            .headers(headermap)
            .json(&tracks_request)
            .send()
            .await;

        match res {
            Ok(resp) => {
                self.parse_new_tracks_response(resp, webrtcbin, client)
                    .await
            }
            Err(err) => self.raise_error(err.to_string()),
        }
    }

    async fn parse_new_tracks_response(
        &self,
        response: reqwest::Response,
        webrtcbin: &gst::Element,
        _client: reqwest::Client,
    ) {
        gst::debug!(CAT, imp = self, "response status: {}", response.status());

        let tracks = match response.status() {
            StatusCode::OK | StatusCode::CREATED => match response.json::<TracksResponse>().await {
                Ok(tracks) => tracks,
                Err(e) => {
                    self.raise_error(e.to_string());
                    return;
                }
            },

            s => {
                match response.bytes().await {
                    Ok(r) => {
                        let res = r.escape_ascii().to_string();

                        // FIXME: Check and handle 'Retry-After' header in case of server error
                        self.raise_error(format!("Unexpected response: {} - {}", s.as_str(), res));
                    }
                    Err(err) => self.raise_error(err.to_string()),
                }
                return;
            }
        };
        gst::debug!(CAT, imp = self, "response body: {tracks:?}");
        if tracks.requires_immediate_renegotiation {
            self.raise_error(
                "TracksResponse requires renegotiation which is currently not supported"
                    .to_string(),
            );
            return;
        }

        for track in tracks.tracks.iter() {
            if let TrackOrError::Error(err) = track {
                self.raise_error(format!(
                    "Posting track produced an error: {} - {}",
                    err.error_code, err.error_description
                ));
                return;
            }
        }

        {
            let mut error = false;
            let track_names = tracks.tracks.iter().fold(vec![], move |mut tracks, track| {
                if error {
                    return tracks;
                }
                let track = match track {
                    TrackOrError::Error(err) => {
                        error = true;
                        self.raise_error(format!(
                            "Posting track produced an error: {} - {}",
                            err.error_code, err.error_description
                        ));
                        return tracks;
                    }
                    TrackOrError::Track(track) => track,
                };
                tracks.push(track.track_name.clone());
                tracks
            });
            let mut settings = self.settings.lock().unwrap();
            settings.tracks = track_names;
        }

        let answer_desc =
            gst_webrtc::WebRTCSessionDescription::try_from(tracks.session_description).unwrap();
        webrtcbin.emit_by_name::<()>(
            "set-remote-description",
            &[&answer_desc, &None::<gst::Promise>],
        );

        self.obj().notify("tracks");
    }
}

impl SignallableImpl for CallsClient {
    fn start(&self) {
        gst::debug!(CAT, imp = self, "Connecting");

        let settings = self.settings.lock().unwrap();
        drop(settings);

        self.obj().connect_closure(
            "webrtcbin-ready",
            false,
            glib::closure!(|signaller: &super::CloudflareCallsProducerSignaller,
                            _consumer_identifier: &str,
                            webrtcbin: &gst::Element| {
                let obj_weak = signaller.downgrade();
                webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _pspec| {
                    let Some(obj) = obj_weak.upgrade() else {
                        return;
                    };

                    let state =
                        webrtcbin.property::<WebRTCICEGatheringState>("ice-gathering-state");

                    match state {
                        WebRTCICEGatheringState::Gathering => {
                            gst::info!(CAT, obj = obj, "ICE gathering started");
                        }
                        WebRTCICEGatheringState::Complete => {
                            gst::info!(CAT, obj = obj, "ICE gathering complete");

                            let webrtcbin = webrtcbin.clone();

                            RUNTIME.spawn(async move {
                                obj.imp().producer_send_offer(&webrtcbin).await
                            });
                        }
                        _ => (),
                    }
                });

                let obj_weak = signaller.downgrade();
                webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _pspec| {
                    let Some(obj) = obj_weak.upgrade() else {
                        return;
                    };

                    let state = webrtcbin
                        .property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
                    match state {
                        gst_webrtc::WebRTCICEConnectionState::Connected
                        | gst_webrtc::WebRTCICEConnectionState::Completed => {
                            let webrtcbin = webrtcbin.clone();
                            RUNTIME.spawn(async move {
                                obj.imp().producer_post_tracks(&webrtcbin).await
                            });
                        }
                        gst_webrtc::WebRTCICEConnectionState::Failed => {
                            obj.imp().raise_error("ICE failed to connect".to_string());
                        }
                        _ => (),
                    }
                });
            }),
        );

        self.obj().emit_by_name::<()>(
            "session-requested",
            &[
                &"unique",
                &"unique",
                &None::<gst_webrtc::WebRTCSessionDescription>,
            ],
        );
    }

    fn stop(&self) {
        if let Some(canceller) = &*self.canceller.lock().unwrap() {
            canceller.abort();
        }
    }

    fn end_session(&self, session_id: &str) {
        assert_eq!(session_id, "unique");

        if let Some(canceller) = &*self.canceller.lock().unwrap() {
            canceller.abort();
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for CallsClient {
    const NAME: &'static str = "GstCloudflareCallsWebRTCSinkSignaller";
    type Type = super::CloudflareCallsProducerSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for CallsClient {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("manual-sdp-munging")
                    .nick("Manual SDP munging")
                    .blurb("Whether the signaller manages SDP munging itself")
                    .default_value(true)
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("server-url")
                    .nick("Server URL")
                    .blurb("The URL of the Cloudflare Calls server")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("app-id")
                    .nick("Application ID")
                    .blurb("Application ID for the Calls API)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("auth-token")
                    .nick("Authorization Token")
                    .blurb("Authentication token to use")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("timeout")
                    .nick("Timeout")
                    .blurb("Value in seconds to timeout requests")
                    .maximum(3600)
                    .minimum(1)
                    .default_value(DEFAULT_TIMEOUT)
                    .build(),
                glib::ParamSpecString::builder("session-id")
                    .nick("Session ID")
                    .blurb("The session ID to use when consuming a stream")
                    .mutable_ready()
                    .build(),
                gst::ParamSpecArray::builder("tracks")
                    .nick("Tracks")
                    .blurb("The list of track IDs to used by this instance")
                    .read_only()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("extra-fields")
                    .nick("Extra JSON fields")
                    .blurb("Any extra fields to add to all requests to the server")
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "server-url" => {
                settings.server_url = value.get().unwrap();
            }
            "app-id" => {
                settings.app_id = value.get().unwrap();
            }
            "auth-token" => {
                settings.auth_token = value.get().unwrap();
            }
            "tracks" => settings.tracks = value.get().unwrap(),
            "extra-fields" => settings.extra_fields = value.get().unwrap(),
            "session-id" => settings.session_id = value.get().unwrap(),
            "timeout" => settings.timeout = value.get().unwrap(),
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "manual-sdp-munging" => true.to_value(),
            "server-url" => settings.server_url.to_value(),
            "app-id" => settings.app_id.to_value(),
            "auth-token" => settings.auth_token.to_value(),
            "tracks" => settings.tracks.to_value(),
            "extra-fields" => settings.extra_fields.to_value(),
            "session-id" => settings.session_id.to_value(),
            "timeout" => settings.timeout.to_value(),
            _ => unimplemented!(),
        }
    }
}
