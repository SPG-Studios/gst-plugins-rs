// SPDX-License-Identifier: MPL-2.0

/// The default protocol used by the signalling server
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SdpOffer {
    #[serde(rename = "type")]
    pub type_: String,
    pub sdp: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: u32,
    pub username_fragment: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IncomingMessage {
    pub message_type: String,
    pub message_payload: String,
    #[serde(default)]
    pub sender_client_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SdpAnswer {
    #[serde(rename = "type")]
    pub type_: String,
    pub sdp: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingIceCandidate {
    pub candidate: String,
    pub sdp_mid: String,
    pub sdp_m_line_index: u32,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingMessage {
    pub action: String,
    pub message_payload: String,
    pub recipient_client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponseMessage {
    pub message_type: String,
    pub status_response: StatusResponseDetail,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatusResponseDetail {
    pub correlation_id: String,
    pub error_type: String,
    pub status_code: String,
    pub description: String,
}
