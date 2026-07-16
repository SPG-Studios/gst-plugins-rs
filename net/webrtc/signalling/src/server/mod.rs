// SPDX-License-Identifier: MPL-2.0

pub use async_tungstenite;

use anyhow::Error;
use async_tungstenite::tungstenite::{
    Message as WsMessage, Utf8Bytes,
    handshake::server::{Callback, NoCallback},
};
use futures::channel::mpsc;
use futures::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task;
use tracing::{debug, error, info, instrument, trace, warn};

struct Peer {
    receive_task_handle: task::JoinHandle<()>,
    send_task_handle: task::JoinHandle<Result<(), Error>>,
    sender: mpsc::Sender<String>,
}

struct State {
    tx: Option<mpsc::Sender<(String, Option<Utf8Bytes>)>>,
    peers: HashMap<String, Peer>,
    /// Per-peer local socket IP (the address the peer connected to). Used to
    /// rewrite ICE/SDP IPs in outgoing messages when `rewrite_internal_ip` is
    /// configured.
    peer_local_ips: HashMap<String, IpAddr>,
    /// When set, outgoing ICE candidates and SDP payloads are scanned for this
    /// IP (in string form) and replaced with the destination peer's
    /// `peer_local_ips` entry. Enables NAT / multi-homed traversal without an
    /// external proxy.
    rewrite_internal_ip: Option<String>,
}

#[derive(Clone)]
pub struct Server {
    state: Arc<Mutex<State>>,
}

/// Rewrite ICE candidate and SDP IP occurrences inside an outgoing
/// signalling JSON message.
///
/// Looks for the well-known protocol fields:
///   - `ice.candidate`              (PeerMessageInner::Ice)
///   - `sdp.sdp`                    (PeerMessageInner::Sdp, both Offer and Answer)
///   - top-level `offer`            (OutgoingMessage::StartSession.offer)
///
/// Returns `Some(rewritten_json)` if any field was modified, otherwise `None`.
/// Operating on `serde_json::Value` keeps the function independent of the
/// concrete `OutgoingMessage` type so `Server::spawn` can stay generic over `O`.
fn rewrite_ice_sdp_in_json(msg_str: &str, internal: &str, external: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(msg_str).ok()?;
    let obj = value.as_object_mut()?;
    let mut modified = false;

    if let Some(ice) = obj.get_mut("ice").and_then(|v| v.as_object_mut())
        && let Some(c) = ice.get("candidate").and_then(|c| c.as_str())
        && c.contains(internal)
    {
        let replaced = c.replace(internal, external);
        ice.insert("candidate".into(), serde_json::Value::String(replaced));
        modified = true;
    }

    if let Some(sdp) = obj.get_mut("sdp").and_then(|v| v.as_object_mut())
        && let Some(s) = sdp.get("sdp").and_then(|s| s.as_str())
        && s.contains(internal)
    {
        let replaced = s.replace(internal, external);
        sdp.insert("sdp".into(), serde_json::Value::String(replaced));
        modified = true;
    }

    if let Some(s) = obj.get("offer").and_then(|s| s.as_str())
        && s.contains(internal)
    {
        let replaced = s.replace(internal, external);
        obj.insert("offer".into(), serde_json::Value::String(replaced));
        modified = true;
    }

    if modified {
        serde_json::to_string(&value).ok()
    } else {
        None
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("error during handshake {0}")]
    Handshake(#[from] async_tungstenite::tungstenite::Error),
    #[error("error during TLS handshake {0}")]
    TLSHandshake(#[from] std::io::Error),
    #[error("timeout during TLS handshake {0}")]
    TLSHandshakeTimeout(#[from] tokio::time::error::Elapsed),
}

impl Server {
    #[instrument(level = "debug", skip(factory))]
    pub fn spawn<
        I: for<'a> Deserialize<'a>,
        O: Serialize + std::fmt::Debug + Send + Sync,
        Factory: FnOnce(Pin<Box<dyn Stream<Item = (String, Option<I>)> + Send>>) -> St,
        St: Stream<Item = (String, O)> + Send + Unpin + 'static,
    >(
        factory: Factory,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<(String, Option<Utf8Bytes>)>(1000);
        let mut handler = factory(Box::pin(rx.filter_map(|(peer_id, msg)| async move {
            if let Some(msg) = msg {
                match serde_json::from_str::<I>(&msg) {
                    Ok(msg) => Some((peer_id, Some(msg))),
                    Err(err) => {
                        warn!("Failed to parse incoming message: {} ({})", err, msg);
                        None
                    }
                }
            } else {
                Some((peer_id, None))
            }
        })));

        let state = Arc::new(Mutex::new(State {
            tx: Some(tx),
            peers: HashMap::new(),
            peer_local_ips: HashMap::new(),
            rewrite_internal_ip: None,
        }));

        let state_clone = state.clone();
        task::spawn(async move {
            while let Some((peer_id, msg)) = handler.next().await {
                let (sender, local_ip, rewrite_internal_ip) = {
                    let mut state = state_clone.lock().unwrap();
                    let sender = state.peers.get_mut(&peer_id).map(|p| p.sender.clone());
                    let local_ip = state.peer_local_ips.get(&peer_id).copied();
                    let rewrite_internal_ip = state.rewrite_internal_ip.clone();
                    (sender, local_ip, rewrite_internal_ip)
                };

                match serde_json::to_string(&msg) {
                    Ok(mut msg_str) => {
                        if let (Some(internal), Some(external_ip)) =
                            (&rewrite_internal_ip, local_ip)
                        {
                            let external = external_ip.to_string();
                            if external != *internal && msg_str.contains(internal.as_str()) {
                                if let Some(new_str) =
                                    rewrite_ice_sdp_in_json(&msg_str, internal, &external)
                                {
                                    info!(
                                        peer_id = %peer_id,
                                        "Rewrote ICE/SDP IP {} -> {} in outgoing message",
                                        internal, external
                                    );
                                    msg_str = new_str;
                                }
                            }
                        }

                        if let Some(mut sender) = sender {
                            trace!("Sending {}", msg_str);
                            let _ = sender.send(msg_str).await;
                        }
                    }
                    Err(err) => {
                        warn!("Failed to serialize outgoing message: {}", err);
                    }
                }
            }
        });

        Self { state }
    }

    /// Enable per-connection ICE/SDP IP rewriting.
    ///
    /// When `internal_ip` is `Some`, any occurrence of that IP (as a string) in
    /// outgoing ICE candidates and SDP payloads is replaced with the local
    /// socket address that the destination peer connected to (captured via
    /// [`Self::accept_async_with_local_addr`]). This makes ICE work in
    /// multi-homed / NAT setups where the producer advertises an internal
    /// address that consumers on another network cannot route to.
    pub fn with_rewrite_ip(self, internal_ip: Option<IpAddr>) -> Self {
        self.state.lock().unwrap().rewrite_internal_ip = internal_ip.map(|ip| ip.to_string());
        self
    }

    #[instrument(level = "debug", skip(state))]
    fn remove_peer(state: Arc<Mutex<State>>, peer_id: &str) {
        let removed = {
            let mut state = state.lock().unwrap();
            state.peer_local_ips.remove(peer_id);
            state.peers.remove(peer_id)
        };
        if let Some(mut peer) = removed {
            let peer_id = peer_id.to_string();
            task::spawn(async move {
                peer.sender.close_channel();
                if let Err(err) = peer.send_task_handle.await {
                    trace!(peer_id = %peer_id, "Error while joining send task: {}", err);
                }

                if let Err(err) = peer.receive_task_handle.await {
                    trace!(peer_id = %peer_id, "Error while joining receive task: {}", err);
                }
            });
        }
    }

    #[instrument(level = "debug", skip(self, stream, callback))]
    pub async fn accept_hdr_async<
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        C: Callback + Unpin,
    >(
        &mut self,
        stream: S,
        callback: C,
    ) -> Result<String, ServerError> {
        self.accept_hdr_async_with_local_addr(stream, callback, None)
            .await
    }

    /// Same as [`Self::accept_hdr_async`] but additionally records the local
    /// socket IP that the peer connected to. The recorded IP is used by the
    /// optional ICE/SDP rewriter (see [`Self::with_rewrite_ip`]).
    #[instrument(level = "debug", skip(self, stream, callback))]
    pub async fn accept_hdr_async_with_local_addr<
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        C: Callback + Unpin,
    >(
        &mut self,
        stream: S,
        callback: C,
        local_addr: Option<IpAddr>,
    ) -> Result<String, ServerError> {
        let ws = match async_tungstenite::tokio::accept_hdr_async(stream, callback).await {
            Ok(ws) => ws,
            Err(err) => {
                warn!("Error during the websocket handshake: {}", err);
                return Err(ServerError::Handshake(err));
            }
        };

        let this_id = uuid::Uuid::new_v4().to_string();
        info!(this_id = %this_id, "New WebSocket connection");

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<String>(1000);

        let this_id_clone = this_id.clone();
        let (mut ws_sink, mut ws_stream) = ws.split();
        let send_task_handle = task::spawn(async move {
            let mut res = Ok(());
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    websocket_receiver.next(),
                )
                .await
                {
                    Ok(Some(msg)) => {
                        trace!(this_id = %this_id_clone, "sending {}", msg);
                        res = ws_sink.send(WsMessage::text(msg)).await;
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(_) => {
                        trace!(this_id = %this_id_clone, "timeout, sending ping");
                        res = ws_sink.send(WsMessage::Ping(Default::default())).await;
                    }
                }

                if let Err(ref err) = res {
                    error!(this_id = %this_id_clone, "Quitting send loop: {err}");
                    break;
                }
            }

            debug!(this_id = %this_id_clone, "Done sending");

            let _ = ws_sink.close(None).await;

            res.map_err(Into::into)
        });

        let mut tx = self.state.lock().unwrap().tx.clone();
        let this_id_clone = this_id.clone();
        let state_clone = self.state.clone();
        let receive_task_handle = task::spawn(async move {
            if let Some(tx) = tx.as_mut()
                && let Err(err) = tx
                    .send((
                        this_id_clone.clone(),
                        Some(
                            serde_json::json!({
                                "type": "newPeer",
                            })
                            .to_string()
                            .into(),
                        ),
                    ))
                    .await
            {
                warn!(this = %this_id_clone, "Error handling message: {:?}", err);
            }
            while let Some(msg) = ws_stream.next().await {
                info!("Received message {msg:?}");
                match msg {
                    Ok(WsMessage::Text(msg)) => {
                        if let Some(tx) = tx.as_mut()
                            && let Err(err) = tx.send((this_id_clone.clone(), Some(msg))).await
                        {
                            warn!(this = %this_id_clone, "Error handling message: {:?}", err);
                        }
                    }
                    Ok(WsMessage::Close(reason)) => {
                        info!(this_id = %this_id_clone, "connection closed: {:?}", reason);
                        break;
                    }
                    Ok(WsMessage::Pong(_)) => {
                        continue;
                    }
                    Ok(_) => warn!(this_id = %this_id_clone, "Unsupported message type"),
                    Err(err) => {
                        warn!(this_id = %this_id_clone, "recv error: {}", err);
                        break;
                    }
                }
            }

            if let Some(tx) = tx.as_mut() {
                let _ = tx.send((this_id_clone.clone(), None)).await;
            }

            Self::remove_peer(state_clone, &this_id_clone);
        });

        {
            let mut state = self.state.lock().unwrap();
            state.peers.insert(
                this_id.clone(),
                Peer {
                    receive_task_handle,
                    send_task_handle,
                    sender: websocket_sender,
                },
            );
            if let Some(ip) = local_addr {
                state.peer_local_ips.insert(this_id.clone(), ip);
            }
        }

        Ok(this_id)
    }

    #[instrument(level = "debug", skip(self, stream))]
    pub async fn accept_async<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        &mut self,
        stream: S,
    ) -> Result<String, ServerError> {
        self.accept_hdr_async(stream, NoCallback).await
    }

    /// Same as [`Self::accept_async`] but additionally records the local
    /// socket IP that the peer connected to (for the ICE/SDP rewriter).
    #[instrument(level = "debug", skip(self, stream))]
    pub async fn accept_async_with_local_addr<
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    >(
        &mut self,
        stream: S,
        local_addr: Option<IpAddr>,
    ) -> Result<String, ServerError> {
        self.accept_hdr_async_with_local_addr(stream, NoCallback, local_addr)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::rewrite_ice_sdp_in_json;
    use serde_json::json;

    fn parse(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("rewriter produced valid JSON")
    }

    #[test]
    fn rewrites_ice_candidate() {
        let input = json!({
            "type": "peer",
            "sessionId": "abc",
            "ice": {
                "candidate": "candidate:1 1 UDP 2113937151 192.168.2.3 50000 typ host",
                "sdpMLineIndex": 0,
            },
        })
        .to_string();

        let out = rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").expect("rewritten");
        let v = parse(&out);
        assert_eq!(
            v["ice"]["candidate"],
            "candidate:1 1 UDP 2113937151 10.0.0.5 50000 typ host"
        );
        assert_eq!(v["ice"]["sdpMLineIndex"], 0);
        assert_eq!(v["type"], "peer");
    }

    #[test]
    fn rewrites_sdp_offer() {
        let input = json!({
            "type": "peer",
            "sessionId": "abc",
            "sdp": {
                "type": "offer",
                "sdp": "v=0\r\no=- 0 0 IN IP4 192.168.2.3\r\nc=IN IP4 192.168.2.3\r\n",
            },
        })
        .to_string();

        let out = rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").expect("rewritten");
        let v = parse(&out);
        let sdp_text = v["sdp"]["sdp"].as_str().unwrap();
        assert!(sdp_text.contains("c=IN IP4 10.0.0.5"));
        assert!(!sdp_text.contains("192.168.2.3"));
        assert_eq!(v["sdp"]["type"], "offer");
    }

    #[test]
    fn rewrites_sdp_answer() {
        let input = json!({
            "type": "peer",
            "sessionId": "abc",
            "sdp": {
                "type": "answer",
                "sdp": "c=IN IP4 192.168.2.3",
            },
        })
        .to_string();

        let out = rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").expect("rewritten");
        let v = parse(&out);
        assert_eq!(v["sdp"]["sdp"], "c=IN IP4 10.0.0.5");
        assert_eq!(v["sdp"]["type"], "answer");
    }

    #[test]
    fn rewrites_start_session_offer() {
        let input = json!({
            "type": "startSession",
            "peerId": "consumer",
            "sessionId": "abc",
            "offer": "c=IN IP4 192.168.2.3",
        })
        .to_string();

        let out = rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").expect("rewritten");
        let v = parse(&out);
        assert_eq!(v["offer"], "c=IN IP4 10.0.0.5");
    }

    #[test]
    fn no_rewrite_when_internal_ip_absent() {
        let input = json!({
            "type": "peer",
            "sessionId": "abc",
            "ice": {
                "candidate": "candidate:1 1 UDP 2113937151 10.0.0.7 50000 typ host",
                "sdpMLineIndex": 0,
            },
        })
        .to_string();

        assert!(rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").is_none());
    }

    #[test]
    fn no_rewrite_for_unrelated_message() {
        let input = json!({
            "type": "welcome",
            "peerId": "abc",
        })
        .to_string();
        assert!(rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").is_none());
    }

    #[test]
    fn does_not_touch_unrelated_fields_containing_ip() {
        let input = json!({
            "type": "peer",
            "sessionId": "192.168.2.3",
            "ice": {
                "candidate": "candidate:1 1 UDP 2113937151 192.168.2.3 50000 typ host",
                "sdpMLineIndex": 0,
            },
        })
        .to_string();

        let out = rewrite_ice_sdp_in_json(&input, "192.168.2.3", "10.0.0.5").expect("rewritten");
        let v = parse(&out);
        assert_eq!(v["sessionId"], "192.168.2.3");
        assert_eq!(
            v["ice"]["candidate"],
            "candidate:1 1 UDP 2113937151 10.0.0.5 50000 typ host"
        );
    }

    #[test]
    fn invalid_json_returns_none() {
        assert!(rewrite_ice_sdp_in_json("not json", "192.168.2.3", "10.0.0.5").is_none());
    }
}
