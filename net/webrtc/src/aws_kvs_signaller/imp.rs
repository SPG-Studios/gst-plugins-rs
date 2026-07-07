// SPDX-License-Identifier: MPL-2.0

use super::protocol as p;
use crate::RUNTIME;
use crate::signaller::{Signallable, SignallableImpl};
use crate::utils::create_tls_connector;
use anyhow::{Error, anyhow};
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use gst::glib;
use gst::glib::prelude::*;
use gst::subclass::prelude::*;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use tokio::task;

use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_credential_types::{Credentials, provider::ProvideCredentials};
use aws_sdk_kinesisvideo::{
    Client,
    types::{ChannelProtocol, ChannelRole, SingleMasterChannelEndpointConfiguration},
};
use aws_sdk_kinesisvideosignaling::Client as SignalingClient;
use aws_sdk_kinesisvideowebrtcstorage::Client as StorageClient;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use data_encoding::BASE64;
use http::Uri;
use std::time::{Duration, SystemTime};

const DEFAULT_AWS_REGION: &str = "us-east-1";
const DEFAULT_PING_TIMEOUT: i32 = 30;
const JOIN_SESSION_OFFER_TIMEOUT_SECS: u64 = 30;

#[allow(deprecated)]
pub static AWS_BEHAVIOR_VERSION: LazyLock<aws_config::BehaviorVersion> =
    LazyLock::new(aws_config::BehaviorVersion::v2023_11_09);

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "webrtc-aws-kvs-signaller",
        gst::DebugColorFlags::empty(),
        Some("WebRTC AWS KVS signaller"),
    )
});

struct PendingIceCandidate {
    session_id: String,
    sdp_m_line_index: u32,
    sdp_mid: Option<String>,
    candidate: String,
}

#[derive(Default)]
struct State {
    /// Sender for the websocket messages
    websocket_sender: Option<mpsc::Sender<p::OutgoingMessage>>,
    send_task_handle: Option<task::JoinHandle<Result<(), Error>>>,
    receive_task_handle: Option<task::JoinHandle<()>>,
    /// ICE candidates received before the session is ready (before send_sdp is called)
    pending_candidates: Vec<PendingIceCandidate>,
    /// Set to true once send_sdp has been called, meaning webrtcbin is ready for ICE
    session_ready: bool,
    reconnect_task_handle: Option<task::JoinHandle<()>>,
    webrtcbin_ready_handler_id: Option<glib::SignalHandlerId>,
    /// Set to true when we initiate the close (stop/reconnect), to suppress reconnect on our own close frame
    closing: bool,
    offer_timeout_handle: Option<task::JoinHandle<()>>,
    offer_received: bool,
}

#[derive(Clone)]
struct Settings {
    address: Option<String>,
    cafile: Option<PathBuf>,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    channel_name: Option<String>,
    join_storage_session: bool,
    ping_timeout: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            address: Some("ws://127.0.0.1:8443".to_string()),
            cafile: None,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            channel_name: None,
            join_storage_session: false,
            ping_timeout: DEFAULT_PING_TIMEOUT,
        }
    }
}

#[derive(Default)]
pub struct Signaller {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl Signaller {
    fn resolve_sender_client_id(&self, raw: Option<String>) -> String {
        match raw.filter(|id| !id.is_empty()) {
            Some(id) => id,
            None => {
                gst::warning!(
                    CAT,
                    imp = self,
                    "No senderClientId in message, using empty string"
                );
                String::new()
            }
        }
    }

    fn generate_correlation_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{ts}_{count}")
    }

    fn schedule_reconnect(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(handle) = state.reconnect_task_handle.take() {
                handle.abort();
            }
        }

        let weak_imp = self.downgrade();
        let handle = RUNTIME.spawn(async move {
            let mut base_delay_ms: u64 = 100;
            let max_delay_ms: u64 = 10_000;

            loop {
                let jittered_delay = {
                    use rand::RngExt;
                    let wait_ms = std::cmp::min(base_delay_ms, max_delay_ms);
                    let actual_ms = rand::rng().random_range(0..=wait_ms);
                    Duration::from_millis(actual_ms)
                };
                {
                    let Some(imp) = weak_imp.upgrade() else {
                        return;
                    };
                    gst::info!(CAT, imp = imp, "Reconnecting in {:?}", jittered_delay);
                }
                tokio::time::sleep(jittered_delay).await;

                let Some(imp) = weak_imp.upgrade() else {
                    return;
                };

                let (send_handle, recv_handle) = {
                    let mut state = imp.state.lock().unwrap();
                    state.pending_candidates.clear();
                    state.session_ready = false;
                    state.closing = true;
                    state.offer_received = false;
                    if let Some(handle) = state.offer_timeout_handle.take() {
                        handle.abort();
                    }

                    if let Some(sender) = state.websocket_sender.as_mut() {
                        sender.close_channel();
                    }
                    state.websocket_sender.take();

                    (
                        state.send_task_handle.take(),
                        state.receive_task_handle.take(),
                    )
                };

                if let Some(handle) = send_handle {
                    let _ = handle.await;
                }
                if let Some(handle) = recv_handle {
                    let _ = handle.await;
                }

                imp.state.lock().unwrap().closing = false;
                gst::info!(CAT, imp = imp, "Attempting reconnect...");

                match imp.connect().await {
                    Ok(()) => {
                        gst::info!(CAT, imp = imp, "Reconnected successfully");
                        return;
                    }
                    Err(err) => {
                        gst::error!(CAT, imp = imp, "Reconnect failed: {err:?}");
                        base_delay_ms = std::cmp::min(base_delay_ms * 2, max_delay_ms);
                    }
                }
            }
        });

        self.state.lock().unwrap().reconnect_task_handle = Some(handle);
    }

    fn handle_message(&self, msg: async_tungstenite::tungstenite::Utf8Bytes) {
        if let Ok(status) = serde_json::from_str::<p::StatusResponseMessage>(&msg) {
            let error_type = &status.status_response.error_type;
            let status_code = &status.status_response.status_code;

            gst::warning!(
                CAT,
                imp = self,
                "Received STATUS_RESPONSE: correlationId={}, errorType={error_type}, statusCode={status_code}, description={}",
                status.status_response.correlation_id,
                status.status_response.description,
            );

            let join_storage_session = self.settings.lock().unwrap().join_storage_session;
            if join_storage_session && (error_type == "GO_AWAY" || error_type == "RECONNECT_ICE") {
                gst::info!(
                    CAT,
                    imp = self,
                    "Server sent {error_type}, scheduling full reconnect"
                );
                self.schedule_reconnect();
            } else {
                self.obj().emit_by_name::<()>(
                    "error",
                    &[&format!(
                        "KVS signaling error (correlationId={}): {status_code} {error_type} - {}",
                        status.status_response.correlation_id, status.status_response.description,
                    )],
                );
            }
            return;
        }

        if msg.is_empty() {
            gst::trace!(CAT, imp = self, "Received ACK from server");
            return;
        }

        let Ok(msg) = serde_json::from_str::<p::IncomingMessage>(&msg) else {
            gst::log!(CAT, imp = self, "Unknown message from server: [{msg}]");
            return;
        };

        match msg.message_type.as_str() {
            "SDP_OFFER" | "ICE_CANDIDATE" => {}
            other => {
                gst::log!(CAT, imp = self, "Ignoring message type {other}");
                return;
            }
        }

        let sender_client_id = self.resolve_sender_client_id(msg.sender_client_id);

        let payload = match BASE64.decode(&msg.message_payload.into_bytes()) {
            Ok(payload) => payload,
            Err(e) => {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to decode message payload from server: {e}"
                );
                self.obj().emit_by_name::<()>(
                    "error",
                    &[&format!(
                        "{:?}",
                        anyhow!("Failed to decode message payload from server: {e}")
                    )],
                );
                return;
            }
        };
        let payload = String::from_utf8_lossy(&payload);

        match msg.message_type.as_str() {
            "SDP_OFFER" => {
                {
                    let mut state = self.state.lock().unwrap();
                    state.offer_received = true;
                    if let Some(handle) = state.offer_timeout_handle.take() {
                        handle.abort();
                    }
                }
                if let Ok(sdp_msg) = serde_json::from_str::<p::SdpOffer>(&payload) {
                    gst::log!(
                        CAT,
                        "Consumer {} got SDP offer: {}",
                        sender_client_id,
                        sdp_msg.sdp
                    );
                    self.obj().emit_by_name::<()>(
                        "session-requested",
                        &[
                            &sender_client_id,
                            &sender_client_id,
                            &Some(gst_webrtc::WebRTCSessionDescription::new(
                                gst_webrtc::WebRTCSDPType::Offer,
                                gst_sdp::SDPMessage::parse_buffer(sdp_msg.sdp.as_bytes()).unwrap(),
                            )),
                        ],
                    );
                } else {
                    gst::warning!(CAT, imp = self, "Failed to parse SDP_OFFER: {payload}");
                }
            }
            "ICE_CANDIDATE" => {
                if let Ok(ice_msg) = serde_json::from_str::<p::IceCandidate>(&payload) {
                    let session_ready = self.state.lock().unwrap().session_ready;

                    if session_ready {
                        gst::log!(
                            CAT,
                            "Consumer {} got candidate {} for m_line {} and mid {:?}",
                            sender_client_id,
                            ice_msg.candidate,
                            ice_msg.sdp_m_line_index,
                            ice_msg.sdp_mid
                        );
                        self.obj().emit_by_name::<()>(
                            "handle-ice",
                            &[
                                &sender_client_id,
                                &ice_msg.sdp_m_line_index,
                                &ice_msg.sdp_mid,
                                &ice_msg.candidate,
                            ],
                        );
                    } else {
                        gst::info!(
                            CAT,
                            imp = self,
                            "Buffering ICE candidate (session not ready): {} for m_line {}",
                            ice_msg.candidate,
                            ice_msg.sdp_m_line_index
                        );
                        self.state
                            .lock()
                            .unwrap()
                            .pending_candidates
                            .push(PendingIceCandidate {
                                session_id: sender_client_id,
                                sdp_m_line_index: ice_msg.sdp_m_line_index,
                                sdp_mid: ice_msg.sdp_mid,
                                candidate: ice_msg.candidate,
                            });
                    }
                } else {
                    gst::warning!(CAT, imp = self, "Failed to parse ICE_CANDIDATE: {payload}");
                }
            }
            _ => unreachable!(),
        }
    }

    async fn connect(&self) -> Result<(), Error> {
        let settings = self.settings.lock().unwrap().clone();

        let connector = create_tls_connector(settings.cafile.as_ref(), false)
            .map_ok(Some)
            .await?;

        let region = aws_config::meta::region::RegionProviderChain::default_provider()
            .or_else(DEFAULT_AWS_REGION)
            .region()
            .await
            .unwrap();
        let access_key = settings.access_key.as_ref();
        let secret_access_key = settings.secret_access_key.as_ref();
        let session_token = settings.session_token.clone();

        let credentials = match (access_key, secret_access_key) {
            (Some(key), Some(secret_key)) => {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Using provided access and secret access key"
                );
                Ok(Credentials::new(
                    key.clone(),
                    secret_key.clone(),
                    session_token,
                    None,
                    "kvs",
                ))
            }
            _ => {
                gst::debug!(CAT, imp = self, "Using default AWS credentials");
                let cred = DefaultCredentialsChain::builder()
                    .region(region.clone())
                    .build()
                    .await;
                cred.provide_credentials().await
            }
        };

        let credentials = match credentials {
            Err(e) => {
                anyhow::bail!("Failed to retrieve credentials with error {e}");
            }
            Ok(credentials) => credentials,
        };

        let Some(channel_name) = settings.channel_name else {
            anyhow::bail!("Channel name cannot be None!");
        };

        let sdk_config = aws_config::defaults(*AWS_BEHAVIOR_VERSION)
            .credentials_provider(credentials.clone())
            .load()
            .await;

        let client = Client::new(&sdk_config);

        let resp = client
            .describe_signaling_channel()
            .set_channel_name(Some(channel_name.clone()))
            .send()
            .await?;

        let Some(cinfo) = resp.channel_info() else {
            anyhow::bail!("No description found for {channel_name}");
        };

        gst::debug!(CAT, "Channel description: {cinfo:?}");

        let Some(channel_arn) = cinfo.channel_arn() else {
            anyhow::bail!("No channel ARN found for {channel_name}");
        };

        let mut protocols = vec![ChannelProtocol::Wss, ChannelProtocol::Https];
        if settings.join_storage_session {
            protocols.push(ChannelProtocol::Webrtc);
        }

        let config = SingleMasterChannelEndpointConfiguration::builder()
            .set_protocols(Some(protocols))
            .set_role(Some(ChannelRole::Master))
            .build();

        let resp = client
            .get_signaling_channel_endpoint()
            .set_channel_arn(Some(channel_arn.to_string()))
            .set_single_master_channel_endpoint_configuration(Some(config))
            .send()
            .await?;

        gst::debug!(CAT, "Endpoints: {:?}", resp.resource_endpoint_list());

        let endpoint_wss_uri = match resp.resource_endpoint_list().iter().find_map(|endpoint| {
            if endpoint.protocol == Some(ChannelProtocol::Wss) {
                Some(endpoint.resource_endpoint().unwrap().to_owned())
            } else {
                None
            }
        }) {
            Some(endpoint_uri_str) => Uri::from_maybe_shared(endpoint_uri_str).unwrap(),
            None => {
                anyhow::bail!("No WSS endpoint found for {channel_name}");
            }
        };

        let endpoint_https_uri = match resp.resource_endpoint_list().iter().find_map(|endpoint| {
            if endpoint.protocol == Some(ChannelProtocol::Https) {
                Some(endpoint.resource_endpoint().unwrap().to_owned())
            } else {
                None
            }
        }) {
            Some(endpoint_uri_str) => endpoint_uri_str,
            None => {
                anyhow::bail!("No HTTPS endpoint found for {channel_name}");
            }
        };

        let endpoint_webrtc_uri = if settings.join_storage_session {
            resp.resource_endpoint_list().iter().find_map(|endpoint| {
                if endpoint.protocol == Some(ChannelProtocol::Webrtc) {
                    endpoint.resource_endpoint().map(|s| s.to_owned())
                } else {
                    None
                }
            })
        } else {
            None
        };

        gst::debug!(
            CAT,
            "Endpoints: WSS={:?} HTTPS={:?} WebRTC={:?}",
            endpoint_wss_uri,
            endpoint_https_uri,
            endpoint_webrtc_uri
        );

        let signaling_config = aws_sdk_kinesisvideosignaling::config::Builder::from(&sdk_config)
            .endpoint_url(endpoint_https_uri)
            .build();

        let signaling_client = SignalingClient::from_conf(signaling_config);

        let resp = signaling_client
            .get_ice_server_config()
            .set_channel_arn(Some(channel_arn.to_string()))
            .send()
            .await?;

        let ice_servers: Vec<String> = resp
            .ice_server_list()
            .iter()
            .filter_map(|server| {
                Option::zip(server.username(), server.password())
                    .map(|(username, password)| (username, password, server))
            })
            .flat_map(|(username, password, server)| {
                server
                    .uris()
                    .iter()
                    .filter_map(move |uri| {
                        uri.split_once(':').map(|(protocol, host)| {
                            let (timestamp, username) = username.split_once(':').unwrap();

                            format!("{protocol}://{timestamp}%3A{encoded_user_name}:{encoded_password}@{host}",
                                encoded_user_name=url_escape::encode_userinfo(username),
                                encoded_password=url_escape::encode_userinfo(password),
                            )
                        })
                    })
            })
            .collect();

        gst::info!(CAT, "Ice servers: {:?}", ice_servers);

        {
            let mut state = self.state.lock().unwrap();
            if let Some(handler_id) = state.webrtcbin_ready_handler_id.take() {
                self.obj().disconnect(handler_id);
            }
        }

        let handler_id = self.obj().connect_closure(
            "webrtcbin-ready",
            false,
            glib::closure!(|_signaller: &super::AwsKvsSignaller,
                            _consumer_identifier: &str,
                            webrtcbin: &gst::Element| {
                webrtcbin.set_property(
                    "stun-server",
                    format!("stun://stun.kinesisvideo.{DEFAULT_AWS_REGION}.amazonaws.com:443"),
                );
                for ice_server in &ice_servers {
                    let res = webrtcbin.emit_by_name::<bool>("add-turn-server", &[&ice_server]);
                    gst::debug!(CAT, "Added ICE server {ice_server}, res: {res}");
                }
            }),
        );
        self.state.lock().unwrap().webrtcbin_ready_handler_id = Some(handler_id);

        let mut signing_settings = SigningSettings::default();
        signing_settings.signature_location = SignatureLocation::QueryParams;
        signing_settings.expires_in = Some(Duration::from_secs(5 * 60));
        let identity = credentials.clone().into();
        let region_string = region.to_string();
        let signing_params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&region_string)
            .name("kinesisvideo")
            .time(SystemTime::now())
            .settings(signing_settings)
            .build()
            .unwrap()
            .into();
        let transcribe_uri = Uri::builder()
            .scheme("wss")
            .authority(endpoint_wss_uri.authority().unwrap().to_owned())
            .path_and_query(format!(
                "/?X-Amz-ChannelARN={}",
                aws_smithy_http::query::fmt_string(channel_arn)
            ))
            .build()
            .map_err(|err| {
                gst::error!(CAT, imp = self, "Failed to build HTTP request URI: {err}");
                anyhow!("Failed to build HTTP request URI: {err}")
            })?;

        // Convert the HTTP request into a signable request
        let signable_request = SignableRequest::new(
            "GET",
            transcribe_uri.to_string(),
            std::iter::empty(),
            SignableBody::Bytes(&[]),
        )
        .expect("signable request");

        let mut request = http::Request::builder()
            .uri(transcribe_uri)
            .body(aws_smithy_types::body::SdkBody::empty())
            .expect("Failed to build valid request");
        let (signing_instructions, _signature) =
            sign(signable_request, &signing_params)?.into_parts();
        signing_instructions.apply_to_request_http1x(&mut request);

        let url = request.uri().to_string();

        gst::debug!(CAT, "Signed URL: {url}");

        let (ws, _) =
            async_tungstenite::tokio::connect_async_with_tls_connector(url, connector).await?;

        gst::info!(CAT, imp = self, "connected");

        // Channel for asynchronously sending out websocket message
        let (mut ws_sink, mut ws_stream) = ws.split();

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (mut _websocket_sender, mut websocket_receiver) =
            mpsc::channel::<p::OutgoingMessage>(1000);
        let imp = self.downgrade();
        let ping_timeout = settings.ping_timeout;
        let send_task_handle = task::spawn(async move {
            let mut res = Ok(());
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(ping_timeout as u64),
                    websocket_receiver.next(),
                )
                .await
                {
                    Ok(Some(msg)) => {
                        if let Some(imp) = imp.upgrade() {
                            gst::trace!(
                                CAT,
                                imp = imp,
                                "Sending websocket message {}",
                                serde_json::to_string(&msg).unwrap()
                            );
                        }
                        res = ws_sink
                            .send(WsMessage::text(serde_json::to_string(&msg).unwrap()))
                            .await;
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(_) => {
                        res = ws_sink.send(WsMessage::Ping(Default::default())).await;
                    }
                }

                if let Err(ref err) = res {
                    match imp.upgrade() {
                        Some(imp) => {
                            gst::error!(CAT, imp = imp, "Quitting send loop: {err}");
                        }
                        _ => {
                            gst::error!(CAT, "Quitting send loop: {err}");
                        }
                    }

                    break;
                }
            }

            match imp.upgrade() {
                Some(imp) => {
                    gst::debug!(CAT, imp = imp, "Done sending");
                }
                _ => {
                    gst::debug!(CAT, "Done sending");
                }
            }

            let _ = ws_sink.close(None).await;

            res.map_err(Into::into)
        });

        let imp = self.downgrade();
        let join_storage_session = settings.join_storage_session;
        let receive_task_handle = task::spawn(async move {
            while let Some(msg) = tokio_stream::StreamExt::next(&mut ws_stream).await {
                match imp.upgrade() {
                    Some(imp) => match msg {
                        Ok(WsMessage::Text(msg)) => {
                            gst::trace!(CAT, "received message [{msg}]");
                            imp.handle_message(msg);
                        }
                        Ok(WsMessage::Close(reason)) => {
                            gst::info!(CAT, imp = imp, "websocket connection closed: {:?}", reason);
                            let closing = imp.state.lock().unwrap().closing;
                            if join_storage_session && !closing {
                                gst::info!(
                                    CAT,
                                    imp = imp,
                                    "Server closed connection, scheduling reconnect"
                                );
                                imp.schedule_reconnect();
                            } else if !closing {
                                imp.obj().emit_by_name::<()>("shutdown", &[]);
                            }
                            break;
                        }
                        Ok(_) => (),
                        Err(err) => {
                            let closing = imp.state.lock().unwrap().closing;
                            if join_storage_session && !closing {
                                gst::warning!(
                                    CAT,
                                    imp = imp,
                                    "WebSocket error: {err}, scheduling reconnect"
                                );
                                imp.schedule_reconnect();
                            } else if !closing {
                                imp.obj().emit_by_name::<()>(
                                    "error",
                                    &[&format!("{:?}", anyhow!("Error receiving: {err}"))],
                                );
                            }
                            break;
                        }
                    },
                    _ => {
                        break;
                    }
                }
            }

            if let Some(imp) = imp.upgrade() {
                gst::info!(CAT, imp = imp, "Stopped websocket receiving");
            }
        });

        {
            let mut state = self.state.lock().unwrap();
            state.websocket_sender = Some(_websocket_sender);
            state.send_task_handle = Some(send_task_handle);
            state.receive_task_handle = Some(receive_task_handle);
        }

        if settings.join_storage_session {
            let webrtc_endpoint = endpoint_webrtc_uri
                .as_deref()
                .ok_or_else(|| anyhow!("No WebRTC endpoint found for {channel_name}"))?;

            gst::info!(
                CAT,
                imp = self,
                "Joining storage session for channel {channel_arn}"
            );

            let storage_config =
                aws_sdk_kinesisvideowebrtcstorage::config::Builder::from(&sdk_config)
                    .endpoint_url(webrtc_endpoint)
                    .build();
            let storage_client = StorageClient::from_conf(storage_config);

            self.state.lock().unwrap().offer_received = false;

            storage_client
                .join_storage_session()
                .channel_arn(channel_arn)
                .send()
                .await
                .map_err(|e| anyhow!("Failed to join storage session: {e:?}"))?;

            gst::info!(CAT, imp = self, "Successfully joined storage session");

            if !self.state.lock().unwrap().offer_received {
                let weak_imp = self.downgrade();
                let offer_timeout = RUNTIME.spawn(async move {
                    tokio::time::sleep(Duration::from_secs(JOIN_SESSION_OFFER_TIMEOUT_SECS)).await;
                    let Some(imp) = weak_imp.upgrade() else {
                        return;
                    };
                    if imp.state.lock().unwrap().closing {
                        return;
                    }
                    gst::warning!(
                        CAT,
                        imp = imp,
                        "No SDP offer received within {}s, scheduling full reconnect",
                        JOIN_SESSION_OFFER_TIMEOUT_SECS
                    );
                    imp.schedule_reconnect();
                });
                self.state.lock().unwrap().offer_timeout_handle = Some(offer_timeout);
            } else {
                gst::debug!(
                    CAT,
                    imp = self,
                    "SDP offer already received during JoinStorageSession call"
                );
            }
        }

        Ok(())
    }
}

impl SignallableImpl for Signaller {
    fn start(&self) {
        let this = self.obj().clone();
        let imp = self.downgrade();
        task::spawn(async move {
            if let Some(imp) = imp.upgrade()
                && let Err(err) = imp.connect().await
            {
                this.emit_by_name::<()>("error", &[&format!("{:?}", anyhow!(err))]);
            }
        });
    }

    fn send_sdp(&self, session_id: &str, sdp: &gst_webrtc::WebRTCSessionDescription) {
        let mut state = self.state.lock().unwrap();

        if !state.session_ready {
            state.session_ready = true;
            let pending = std::mem::take(&mut state.pending_candidates);

            if !pending.is_empty() {
                gst::info!(
                    CAT,
                    imp = self,
                    "Scheduling flush of {} buffered ICE candidates",
                    pending.len()
                );
                let imp = self.downgrade();
                RUNTIME.spawn(async move {
                    if let Some(imp) = imp.upgrade() {
                        for candidate in pending {
                            gst::debug!(
                                CAT,
                                imp = imp,
                                "Flushing buffered ICE candidate: {} for m_line {}",
                                candidate.candidate,
                                candidate.sdp_m_line_index
                            );
                            imp.obj().emit_by_name::<()>(
                                "handle-ice",
                                &[
                                    &candidate.session_id,
                                    &candidate.sdp_m_line_index,
                                    &candidate.sdp_mid,
                                    &candidate.candidate,
                                ],
                            );
                        }
                    }
                });
            }
        }

        let correlation_id = Self::generate_correlation_id();
        gst::debug!(
            CAT,
            imp = self,
            "Sending SDP_ANSWER to {session_id} with correlationId={correlation_id}"
        );

        let msg = p::OutgoingMessage {
            action: "SDP_ANSWER".to_string(),
            message_payload: BASE64.encode(
                &serde_json::to_string(&p::SdpAnswer {
                    type_: "answer".to_string(),
                    sdp: sdp.sdp().as_text().unwrap(),
                })
                .unwrap()
                .into_bytes(),
            ),
            recipient_client_id: session_id.to_string(),
            correlation_id: Some(correlation_id),
        };

        if let Some(mut sender) = state.websocket_sender.clone() {
            let imp = self.downgrade();
            RUNTIME.spawn(async move {
                if let Err(err) = sender.send(msg).await
                    && let Some(imp) = imp.upgrade()
                {
                    imp.obj()
                        .emit_by_name::<()>("error", &[&format!("{:?}", anyhow!("Error: {err}"))]);
                }
            });
        }
    }

    fn add_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
        _sdp_mid: Option<String>,
    ) {
        let state = self.state.lock().unwrap();

        let correlation_id = Self::generate_correlation_id();
        let msg = p::OutgoingMessage {
            action: "ICE_CANDIDATE".to_string(),
            message_payload: BASE64.encode(
                &serde_json::to_string(&p::OutgoingIceCandidate {
                    candidate: candidate.to_string(),
                    sdp_mid: sdp_m_line_index.to_string(),
                    sdp_m_line_index,
                })
                .unwrap()
                .into_bytes(),
            ),
            recipient_client_id: session_id.to_string(),
            correlation_id: Some(correlation_id),
        };

        if let Some(mut sender) = state.websocket_sender.clone() {
            let imp = self.downgrade();
            RUNTIME.spawn(async move {
                if let Err(err) = sender.send(msg).await
                    && let Some(imp) = imp.upgrade()
                {
                    imp.obj()
                        .emit_by_name::<()>("error", &[&format!("{:?}", anyhow!("Error: {err}"))]);
                }
            });
        }
    }

    fn stop(&self) {
        gst::info!(CAT, imp = self, "Stopping now");

        let (send_task_handle, receive_task_handle, websocket_sender) = {
            let mut state = self.state.lock().unwrap();
            state.pending_candidates.clear();
            state.session_ready = false;
            state.closing = true;
            if let Some(handle) = state.reconnect_task_handle.take() {
                handle.abort();
            }
            if let Some(handle) = state.offer_timeout_handle.take() {
                handle.abort();
            }
            if let Some(handler_id) = state.webrtcbin_ready_handler_id.take() {
                self.obj().disconnect(handler_id);
            }
            (
                state.send_task_handle.take(),
                state.receive_task_handle.take(),
                state.websocket_sender.take(),
            )
        };
        if let Some(mut sender) = websocket_sender {
            let imp = self.downgrade();
            RUNTIME.block_on(async move {
                sender.close_channel();

                if let Some(handle) = send_task_handle
                    && let Err(err) = handle.await
                    && let Some(imp) = imp.upgrade()
                {
                    gst::warning!(CAT, imp = imp, "Error while joining send task: {err}");
                }

                if let Some(handle) = receive_task_handle
                    && let Err(err) = handle.await
                    && let Some(imp) = imp.upgrade()
                {
                    gst::warning!(CAT, imp = imp, "Error while joining receive task: {err}");
                }
            });
        }
    }

    fn end_session(&self, session_id: &str) {
        gst::info!(CAT, imp = self, "Signalling session {session_id} ended");

        let closing = self.state.lock().unwrap().closing;
        let join_storage_session = self.settings.lock().unwrap().join_storage_session;
        if !join_storage_session || closing {
            return;
        }

        gst::info!(
            CAT,
            imp = self,
            "Storage session ended, scheduling full reconnect for fresh ICE config"
        );
        self.schedule_reconnect();
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Signaller {
    const NAME: &'static str = "GstAwsKvsWebRTCSinkSignaller";
    type Type = super::AwsKvsSignaller;
    type ParentType = glib::Object;
    type Interfaces = (Signallable,);
}

impl ObjectImpl for Signaller {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecBoolean::builder("manual-sdp-munging")
                    .nick("Manual SDP munging")
                    .blurb("Whether the signaller manages SDP munging itself")
                    .default_value(false)
                    .read_only()
                    .build(),
                glib::ParamSpecString::builder("address")
                    .nick("Address")
                    .blurb("Address of the signalling server")
                    .default_value("ws://127.0.0.1:8443")
                    .build(),
                glib::ParamSpecString::builder("cafile")
                    .nick("CA file")
                    .blurb("Path to a Certificate file to add to the set of roots the TLS connector will trust")
                    .build(),
                glib::ParamSpecString::builder("channel-name")
                    .nick("Channel name")
                    .blurb("Name of the channel to connect as master to")
                    .build(),
                glib::ParamSpecBoolean::builder("join-storage-session")
                    .nick("Join storage session")
                    .blurb("When true, call JoinStorageSession so media is stored in Kinesis Video Streams. Enforces H.264 video and Opus audio.")
                    .default_value(false)
                    .build(),
                glib::ParamSpecInt::builder("ping-timeout")
                    .nick("Ping Timeout")
                    .blurb("How often (in seconds) to send pings to keep the websocket alive")
                    .default_value(DEFAULT_PING_TIMEOUT)
                    .minimum(1)
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "address" => {
                let address: Option<_> = value.get().unwrap();

                if let Some(address) = address {
                    gst::info!(CAT, "Signaller address set to {address}");

                    let mut settings = self.settings.lock().unwrap();
                    settings.address = Some(address);
                } else {
                    gst::error!(CAT, "address can't be None");
                }
            }
            "cafile" => {
                let value: String = value.get().unwrap();
                let mut settings = self.settings.lock().unwrap();
                settings.cafile = Some(value.into());
            }
            "access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.access_key = value.get().unwrap();
            }
            "secret-access-key" => {
                let mut settings = self.settings.lock().unwrap();
                settings.secret_access_key = value.get().unwrap();
            }
            "session-token" => {
                let mut settings = self.settings.lock().unwrap();
                settings.session_token = value.get().unwrap();
            }
            "channel-name" => {
                let mut settings = self.settings.lock().unwrap();
                settings.channel_name = value.get().unwrap();
            }
            "join-storage-session" => {
                let mut settings = self.settings.lock().unwrap();
                settings.join_storage_session = value.get().unwrap();
            }
            "ping-timeout" => {
                let mut settings = self.settings.lock().unwrap();
                settings.ping_timeout = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "manual-sdp-munging" => false.to_value(),
            "address" => self.settings.lock().unwrap().address.to_value(),
            "cafile" => {
                let settings = self.settings.lock().unwrap();
                let cafile = settings.cafile.as_ref();
                cafile.and_then(|file| file.to_str()).to_value()
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
            "channel-name" => self.settings.lock().unwrap().channel_name.to_value(),
            "join-storage-session" => self
                .settings
                .lock()
                .unwrap()
                .join_storage_session
                .to_value(),
            "ping-timeout" => self.settings.lock().unwrap().ping_timeout.to_value(),
            _ => unimplemented!(),
        }
    }
}
