// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use glib::{ParamSpec, ParamSpecString, Value};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use rsa::pkcs1v15::{Signature as RsaSignature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier as _;
use rsa::RsaPublicKey;
use rustls_pemfile::certs;
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};
use x509_parser::prelude::*;

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;
use std::sync::LazyLock;
use std::collections::{HashMap, VecDeque};
use url::Url;

use anyhow::Result;

use crate::common::HashMethod;
use crate::nal_parser::{NalParser, NalUnits, VideoCodec};
use crate::dsc_substream::DscSubstreamManager;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "dscverifier",
        gst::DebugColorFlags::empty(),
        Some("GstDscVerifier"),
    )
});

#[derive(Debug, Clone, Copy, PartialEq)]
enum CertVerificationStatus {
    Unverified,
    Verified,
    Untrusted,
    Error,
}

struct GopVerificationState {
    dsc_manager: Option<DscSubstreamManager>,
    gop_started: bool,
    nal_parser: Option<NalParser>,
    public_key_cache: HashMap<String, RsaPublicKey>,
    cert_verification_cache: HashMap<String, CertVerificationStatus>,
    current_hash_method: Option<HashMethod>,
    current_cert_uri: Option<String>,
    current_cert_status: CertVerificationStatus,
}

impl Default for GopVerificationState {
    fn default() -> Self {
        Self {
            dsc_manager: None,
            gop_started: false,
            nal_parser: None,
            public_key_cache: HashMap::new(),
            cert_verification_cache: HashMap::new(),
            current_hash_method: None,
            current_cert_uri: None,
            current_cert_status: CertVerificationStatus::Unverified,
        }
    }
}

struct Settings {
    key_store_path: Option<String>,
    trust_store_path: Option<String>,
    fail_on_verification_error: bool,
    buffer: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            key_store_path: None,
            trust_store_path: None,
            fail_on_verification_error: false,
            buffer: true,
        }
    }
}

#[derive(Default)]
struct State {
    gop_state: GopVerificationState,
    sinkpad: Option<gst::Pad>,
    srcpad: Option<gst::Pad>,
    /// Buffered access units when `buffer=true` (not yet output)
    queued_buffers: VecDeque<gst::Buffer>,
    /// Measured GOP duration, updated after each drained GOP (used for latency reporting)
    gop_latency: Option<gst::ClockTime>,
}

#[derive(Default)]
pub struct DscVerifier {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for DscVerifier {
    const NAME: &'static str = "GstDscVerifier";
    type Type = super::DscVerifier;
    type ParentType = gst::Element;
}

impl ObjectImpl for DscVerifier {
    fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
        match pspec.name() {
            "key-store-path" => {
                let path = value.get::<&str>().unwrap();
                self.settings.lock().unwrap().key_store_path = Some(path.to_string());
                gst::info!(CAT, "Set key-store-path property to {}", path);
            }
            "trust-store-path" => {
                let path = value.get::<&str>().unwrap();
                self.settings.lock().unwrap().trust_store_path = Some(path.to_string());
                gst::info!(CAT, "Set trust-store-path property to {}", path);
            }
            "fail-on-verification-error" => {
                let fail = value.get::<bool>().unwrap();
                self.settings.lock().unwrap().fail_on_verification_error = fail;
                gst::info!(CAT, "Set fail-on-verification-error property to {}", fail);
            }
            "buffer" => {
                let buffer = value.get::<bool>().unwrap();
                self.settings.lock().unwrap().buffer = buffer;
                gst::info!(CAT, "Set buffer property to {}", buffer);
            }
            _ => {}
        }
    }

    fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "key-store-path" => settings.key_store_path.clone().to_value(),
            "trust-store-path" => settings.trust_store_path.clone().to_value(),
            "fail-on-verification-error" => settings.fail_on_verification_error.to_value(),
            "buffer" => settings.buffer.to_value(),
            _ => Value::from_type(pspec.value_type()),
        }
    }

    fn properties() -> &'static [ParamSpec] {
        static PROPERTIES: LazyLock<Vec<ParamSpec>> = LazyLock::new(|| vec![
            ParamSpecString::builder("key-store-path")
                .nick("Key Store Path")
                .blurb("Directory path where public key files are stored (keyStoreDir)")
                .readwrite()
                .build(),
            ParamSpecString::builder("trust-store-path")
                .nick("Trust Store Path")
                .blurb("Directory path where trusted CA certificates are stored (trustStoreDir)")
                .readwrite()
                .build(),
            glib::ParamSpecBoolean::builder("fail-on-verification-error")
                .nick("Fail on Verification Error")
                .blurb("Whether to fail the pipeline when signature verification fails (default: false)")
                .default_value(false)
                .readwrite()
                .build(),
            glib::ParamSpecBoolean::builder("buffer")
                .nick("Buffer GOP")
                .blurb("Whether to buffer the full GOP before outputting after verification (default: true)")
                .default_value(true)
                .build(),
        ]);
        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        let class = obj.class();
        let templ = class.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .name("sink")
            .chain_function(|pad, parent, buffer| {
                DscVerifier::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |verifier| verifier.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                DscVerifier::catch_panic_pad_function(
                    parent,
                    || false,
                    |verifier| verifier.sink_event(pad, event),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                DscVerifier::catch_panic_pad_function(
                    parent,
                    || gst::Pad::iterate_internal_links_default(pad, parent),
                    |verifier| verifier.iterate_internal_links(pad),
                )
            })
            .flags(gst::PadFlags::PROXY_CAPS)
            .build();
        obj.add_pad(&sinkpad).unwrap();

        let templ = class.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .name("src")
            .query_function(|pad, parent, query| {
                DscVerifier::catch_panic_pad_function(
                    parent,
                    || false,
                    |verifier| verifier.src_query(pad, query),
                )
            })
            .iterate_internal_links_function(|pad, parent| {
                DscVerifier::catch_panic_pad_function(
                    parent,
                    || gst::Pad::iterate_internal_links_default(pad, parent),
                    |verifier| verifier.iterate_internal_links(pad),
                )
            })
            .build();
        obj.add_pad(&srcpad).unwrap();

        let mut state = self.state.lock().unwrap();
        state.sinkpad = Some(sinkpad);
        state.srcpad = Some(srcpad);
    }
}

impl GstObjectImpl for DscVerifier {}

impl ElementImpl for DscVerifier {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "DSC Verifier",
                "Generic",
                "Verifies video buffer signatures using metadata-provided keys",
                "Diego Nieto <dnieto@fluendo.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = [
                gst::Structure::builder("video/x-h265")
                    .field("alignment", "au")
                    .field("stream-format", gst::List::new(["byte-stream", "hvc1", "hev1"]))
                    .build(),
                gst::Structure::builder("video/x-h266")
                    .field("alignment", "au")
                    .field("stream-format", gst::List::new(["byte-stream", "vvc1", "vvi1"]))
                    .build(),
            ]
            .into_iter()
            .collect::<gst::Caps>();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                ).unwrap(),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                ).unwrap(),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl DscVerifier {
    fn resolve_cert_path(&self, cert_uri: &str, key_store_path: Option<&str>) -> String {
        if let Some(key_store_path) = key_store_path {
            let key_store_dir = Path::new(key_store_path);

            let file_name = match Url::parse(cert_uri) {
                Ok(url) => {
                    if url.scheme() != "file" {
                        gst::warning!(CAT, imp = self, "Unsupported URI scheme '{}' in key-source-uri '{}', using basename", url.scheme(), cert_uri);
                    }

                    url.path_segments()
                        .and_then(|segments| segments.filter(|s| !s.is_empty()).last())
                        .map(|s| s.to_string())
                }
                Err(_) => Path::new(cert_uri)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string()),
            }
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| cert_uri.to_string());

            let final_path = key_store_dir.join(&file_name);
            gst::debug!(CAT, imp = self, "Resolved certificate file '{}' from key_source_uri '{}'", file_name, cert_uri);
            return final_path.to_string_lossy().to_string();
        }

        match Url::parse(cert_uri) {
            Ok(url) if url.scheme() == "file" => {
                if let Ok(path) = url.to_file_path() {
                    return path.to_string_lossy().to_string();
                }
            }
            Ok(_) | Err(_) => {}
        }

        cert_uri.to_string()
    }

    fn verify_certificate(
        &self,
        cert_der: &[u8],
        cert_path: &str,
    ) -> CertVerificationStatus {
        gst::trace!(CAT, imp = self, "verify_certificate: called for cert_path={}", cert_path);
        let trust_store_path = self.settings.lock().unwrap().trust_store_path.clone();

        if trust_store_path.is_none() {
            gst::warning!(CAT, imp = self, "No trust store path configured, skipping certificate validation");
            return CertVerificationStatus::Unverified;
        }

        let trust_dir = trust_store_path.as_ref().unwrap();
        gst::debug!(CAT, imp = self, "Verifying certificate against trust store: {}", trust_dir);

        let (_, cert) = match parse_x509_certificate(cert_der) {
            Ok(parsed) => parsed,
            Err(e) => {
                gst::error!(CAT, imp = self, "Failed to parse certificate at {}: {}", cert_path, e);
                return CertVerificationStatus::Error;
            }
        };

        let subject = cert.subject().to_string();
        gst::info!(CAT, imp = self, "Certificate Subject: {}", subject);

        // Try to load certificates from the trust store directory
        let trust_path = Path::new(trust_dir);
        gst::trace!(CAT, imp = self, "verify_certificate: trust_path={:?}", trust_path);
        if trust_path.is_dir() {
            // Read all .crt and .pem files from trust store
            match fs::read_dir(trust_path) {
                Ok(entries) => {
                    let mut trusted = false;
                    for entry in entries.flatten() {
                        let path = entry.path();
                        gst::trace!(CAT, imp = self, "verify_certificate: checking CA file {}", path.display());
                        if let Some(ext) = path.extension() {
                            if ext == "crt" || ext == "pem" {
                                if let Ok(ca_data) = fs::read(&path) {
                                    let ca_der = match self.extract_first_certificate_der(&ca_data) {
                                        Ok(der) => der,
                                        Err(e) => {
                                            gst::debug!(CAT, imp = self, "Skipping invalid CA cert {}: {}", path.display(), e);
                                            continue;
                                        }
                                    };

                                    if let Ok((_, ca_cert)) = parse_x509_certificate(&ca_der) {
                                        if ca_cert.subject() == cert.issuer()
                                            && cert.verify_signature(Some(ca_cert.public_key())).is_ok()
                                        {
                                            trusted = true;
                                            gst::trace!(CAT, imp = self, "Trusted issuer/signature match found in {}", path.display());
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if trusted {
                        let issuer = cert.issuer().to_string();
                        gst::info!(CAT, imp = self, "Certificate Issuer (CA): {}", issuer);
                        gst::info!(CAT, imp = self, "\x1b[1;32mCertificate validation passed.\x1b[0m");
                        CertVerificationStatus::Verified
                    } else {
                        gst::warning!(CAT, imp = self, "\x1b[1;31mCertificate validation failed\x1b[0m");
                        CertVerificationStatus::Untrusted
                    }
                }
                Err(e) => {
                    gst::error!(CAT, imp = self, "Failed to read trust store directory: {}", e);
                    CertVerificationStatus::Error
                }
            }
        } else {
            gst::warning!(CAT, imp = self, "Configured trust store path is not a directory: {}", trust_dir);
            CertVerificationStatus::Error
        }
    }

    fn load_public_key_from_cert_uri(
        &self,
        cert_uri: &str,
        gop_state: &mut GopVerificationState,
    ) -> std::result::Result<RsaPublicKey, String> {
        if let Some(cached_key) = gop_state.public_key_cache.get(cert_uri) {
            gst::debug!(CAT, imp = self, "Using cached public key for cert_uri: {}", cert_uri);
            if let Some(&cached_status) = gop_state.cert_verification_cache.get(cert_uri) {
                gop_state.current_cert_status = cached_status;
                gst::debug!(CAT, imp = self, "Restored cached certificate status: {:?}", cached_status);
            }
            return Ok(cached_key.clone());
        }

        gst::info!(CAT, imp = self, "Loading public key from cert_uri: {}", cert_uri);

        let key_store_path = self.settings.lock().unwrap().key_store_path.clone();
        let final_key_path = self.resolve_cert_path(cert_uri, key_store_path.as_deref());

        gst::debug!(CAT, imp = self, "Final key path constructed: {}", final_key_path);

        let cert_status = if let Some(&cached_status) = gop_state.cert_verification_cache.get(cert_uri) {
            gst::debug!(CAT, imp = self, "Using cached certificate verification status for: {}", cert_uri);
            cached_status
        } else {
            CertVerificationStatus::Unverified
        };

        match fs::read(&final_key_path) {
            Ok(cert_data) => {
                let cert_der = self.extract_first_certificate_der(&cert_data)?;

                let (_, cert) = parse_x509_certificate(&cert_der)
                    .map_err(|e| format!("Failed to parse X.509 certificate at {}: {}", final_key_path, e))?;

                let _verification_status = if cert_status == CertVerificationStatus::Unverified {
                    let status = self.verify_certificate(&cert_der, &final_key_path);
                    gop_state.cert_verification_cache.insert(cert_uri.to_string(), status);
                    gop_state.current_cert_status = status;
                    status
                } else {
                    gop_state.current_cert_status = cert_status;
                    cert_status
                };

                let pkey = RsaPublicKey::from_public_key_der(cert.public_key().raw)
                    .map_err(|e| format!("Failed to extract RSA public key from certificate {}: {}", final_key_path, e))?;

                gop_state.public_key_cache.insert(cert_uri.to_string(), pkey.clone());
                gst::info!(CAT, imp = self, "Loaded and cached RSA public key from certificate: {}", final_key_path);
                Ok(pkey)
            },
            Err(e) => {
                gst::error!(CAT, imp = self, "Failed to read certificate file from {}: {}", final_key_path, e);
                Err(format!("Failed to read certificate file from {}: {}", final_key_path, e))
            }
        }
    }

    fn extract_first_certificate_der(
        &self,
        cert_data: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        let mut cursor = Cursor::new(cert_data);
        if let Some(first_cert) = certs(&mut cursor).next() {
            return first_cert
                .map(|c| c.as_ref().to_vec())
                .map_err(|e| format!("Failed to parse PEM certificate: {}", e));
        }

        if cert_data.starts_with(b"-----BEGIN") {
            return Err("No certificate found in PEM data".to_string());
        }

        Ok(cert_data.to_vec())
    }

    fn verify_rsa_pkcs1v15_signature(
        &self,
        hash_method: HashMethod,
        pkey: &RsaPublicKey,
        data_packet: &[u8],
        signature: &[u8],
    ) -> std::result::Result<bool, String> {
        let signature = RsaSignature::try_from(signature)
            .map_err(|e| format!("Invalid RSA PKCS#1 v1.5 signature bytes: {}", e))?;

        match hash_method {
            HashMethod::Sha1 => {
                let verifying_key = VerifyingKey::<Sha1>::new(pkey.clone());
                Ok(verifying_key.verify(data_packet, &signature).is_ok())
            }
            HashMethod::Sha224 => {
                let verifying_key = VerifyingKey::<Sha224>::new(pkey.clone());
                Ok(verifying_key.verify(data_packet, &signature).is_ok())
            }
            HashMethod::Sha256 => {
                let verifying_key = VerifyingKey::<Sha256>::new(pkey.clone());
                Ok(verifying_key.verify(data_packet, &signature).is_ok())
            }
            HashMethod::Sha384 => {
                let verifying_key = VerifyingKey::<Sha384>::new(pkey.clone());
                Ok(verifying_key.verify(data_packet, &signature).is_ok())
            }
            HashMethod::Sha512 => {
                let verifying_key = VerifyingKey::<Sha512>::new(pkey.clone());
                Ok(verifying_key.verify(data_packet, &signature).is_ok())
            }
        }
    }

    fn handle_initialization_meta(
        &self,
        init_meta: &gst_video::video_meta::VideoDSCInitializationMeta,
        gop_state: &mut GopVerificationState,
    ) -> Result<(), gst::FlowError> {
        let obj = self.obj();
        let dsc_init = init_meta.dsc_initialization();

        gst::debug!(CAT, imp = self, "DSC initialization - id: {}, hash_method: {}, key_retrieval_mode: {}",
            dsc_init.id(), dsc_init.hash_method_type(), dsc_init.key_retrieval_mode_idc());

        let hash_method = HashMethod::from(dsc_init.hash_method_type());
        let cert_uri = dsc_init.key_source_uri().map(|s| s.to_string());

        let current_uri = gop_state.current_cert_uri.as_deref();
        let new_uri = cert_uri.as_deref();
        let uri_changed = current_uri != new_uri;
        let is_segment_start = dsc_init.signed_content_start_flag();

        let manager_exists = gop_state.dsc_manager.is_some();
        gst::info!(CAT, imp = self,
            "🔄 State check - manager_exists: {}, Current: {:?}, New: {:?}, Changed: {}, SegmentStart: {}",
            manager_exists, current_uri, new_uri, uri_changed, is_segment_start);

        if manager_exists && !uri_changed {
            gst::info!(CAT, imp = self, "♻️ Continuing existing segment - creating NEW SUBSTREAM for next GOP");

            if let Some(ref mut manager) = gop_state.dsc_manager {
                *manager = match DscSubstreamManager::new(
                    hash_method,
                    dsc_init.hash_method_type(),
                    if dsc_init.content_uuid_present_flag() { Some(*dsc_init.content_uuid()) } else { None },
                ) {
                    Ok(mut new_manager) => {
                        new_manager.last_digest = manager.last_digest.clone();
                        new_manager
                    },
                    Err(e) => {
                        gst::error!(CAT, imp = self, "Failed to create new substream: {}", e);
                        return Err(gst::FlowError::Error);
                    }
                };
            }

            gop_state.gop_started = true;

            return Ok(());
        }

        if manager_exists {
            if uri_changed && is_segment_start {
                gst::warning!(CAT, imp = self,
                    "⚠️ DSC re-initializing with DIFFERENT certificate URI - creating NEW signed segment");
            }
            gst::info!(CAT, imp = self, "🔄 DscSubstreamManager RESET for new segment");
        } else {
            gst::info!(CAT, imp = self, "🆕 First DSC initialization");
        }

        gop_state.dsc_manager = None;
        gop_state.gop_started = false;

        gop_state.current_hash_method = Some(hash_method);
        gop_state.current_cert_uri = cert_uri.clone();

        let content_uuid = if dsc_init.content_uuid_present_flag() {
            Some(*dsc_init.content_uuid())
        } else {
            None
        };

        let new_dsc_manager = match DscSubstreamManager::new(
            hash_method,
            dsc_init.hash_method_type(),
            content_uuid,
        ) {
            Ok(manager) => manager,
            Err(e) => {
                gst::element_error!(obj, gst::CoreError::Failed, ["Failed to create DscSubstreamManager: {}", e]);
                return Err(gst::FlowError::Error);
            }
        };

        gop_state.dsc_manager = Some(new_dsc_manager);
        gop_state.gop_started = true;

        gst::debug!(CAT, imp = self, "DSC parameters - hash_method: {:?}, content_uuid_present: {}, cert_uri: {:?}",
            hash_method, dsc_init.content_uuid_present_flag(), cert_uri);

        Ok(())
    }

    fn extract_nal_units(
        &self,
        buffer: &gst::BufferRef,
        gop_state: &GopVerificationState,
    ) -> Result<NalUnits, gst::FlowError> {
        let obj = self.obj();

        let map = match buffer.map_readable() {
            Ok(m) => m,
            Err(_) => {
                gst::element_error!(obj, gst::CoreError::Failed, ["Failed to map buffer for reading"]);
                return Err(gst::FlowError::Error);
            }
        };

        if let Some(ref nal_parser) = gop_state.nal_parser {
            match nal_parser.extract_signable_data(&map) {
                Ok(nal_units) => {
                    gst::trace!(CAT, imp = self, "Extracted {} NAL units from {} raw bytes",
                        nal_units.len(), map.len());
                    Ok(nal_units)
                },
                Err(e) => {
                    gst::element_error!(obj, gst::CoreError::Failed, ["NAL parsing failed: {}", e]);
                    Err(gst::FlowError::Error)
                }
            }
        } else {
            gst::element_error!(obj, gst::CoreError::Failed, ["No NAL parser available"]);
            Err(gst::FlowError::Error)
        }
    }

    fn add_nal_units_to_substream(
        &self,
        nal_units: &NalUnits,
        substream_id: usize,
        gop_state: &mut GopVerificationState,
        is_verification_buffer: bool,
    ) -> Result<(), gst::FlowError> {
        if let Some(ref mut dsc_manager) = gop_state.dsc_manager {
            for nal_data in nal_units {
                if let Err(e) = dsc_manager.add_to_substream(substream_id, nal_data) {
                    gst::error!(CAT, imp = self, "Failed to add NAL to substream: {}", e);
                    return Err(gst::FlowError::Error);
                }

                if is_verification_buffer {
                    gst::debug!(CAT, imp = self, "Added verification NAL to substream {}, size: {}, first 32: {:02x?}",
                        substream_id, nal_data.len(), &nal_data[..std::cmp::min(32, nal_data.len())]);
                } else {
                    gst::trace!(CAT, imp = self, "Added NAL to substream {}, size: {}, first 32: {:02x?}, last 32: {:02x?}",
                        substream_id, nal_data.len(),
                        &nal_data[..std::cmp::min(32, nal_data.len())],
                        &nal_data[nal_data.len().saturating_sub(32)..]);
                }
            }
        }
        Ok(())
    }

    fn get_substream_id(
        &self,
        selection_meta: Option<&gst_video::video_meta::VideoDSCSelectionMeta>,
        default_id: usize,
    ) -> usize {
        if let Some(sel_meta) = selection_meta {
            let substream = sel_meta.dsc_selection().verification_substream_id() as usize;
            gst::trace!(CAT, imp = self, "Found DSC selection metadata, using substream: {}", substream);
            substream
        } else {
            default_id
        }
    }

    fn get_verification_params(
        &self,
        gop_state: &mut GopVerificationState,
    ) -> Result<(HashMethod, RsaPublicKey), gst::FlowError> {
        let obj = self.obj();

        let hash_method = match gop_state.current_hash_method {
            Some(method) => method,
            None => {
                gst::element_error!(obj, gst::CoreError::Failed, ["No hash method stored from initialization metadata"]);
                return Err(gst::FlowError::Error);
            }
        };

        let cert_uri = gop_state.current_cert_uri.clone();

        let pkey = if let Some(uri) = cert_uri {
            match self.load_public_key_from_cert_uri(&uri, gop_state) {
                Ok(key) => key,
                Err(e) => {
                    gst::error!(CAT, imp = self, "Failed to load public key for verification: {}", e);
                    return Err(gst::FlowError::Error);
                }
            }
        } else {
            gst::element_error!(obj, gst::CoreError::Failed, ["No key_source_uri stored from DSC initialization metadata"]);
            return Err(gst::FlowError::Error);
        };

        Ok((hash_method, pkey))
    }

    fn finalize_and_verify(
        &self,
        dsc_verification: &gst_video::H274DigitallySignedContentVerification,
        substream_id: usize,
        gop_state: &mut GopVerificationState,
    ) -> Result<(), gst::FlowError> {
        let obj = self.obj();

        let (hash_method, pkey) = self.get_verification_params(gop_state)?;
        let cert_status = gop_state.current_cert_status;
        if let Some(ref mut dsc_manager) = gop_state.dsc_manager {
            gst::debug!(CAT, imp = self, "About to create data packet from accumulated substream data");
            match dsc_manager.create_data_packet(substream_id) {
                Ok(data_packet) => {
                    self.verify_signature(
                        dsc_verification,
                        &data_packet,
                        hash_method,
                        &pkey,
                        cert_status,
                    )?;
                },
                Err(e) => {
                    gst::element_error!(obj, gst::CoreError::Failed, ["Failed to create data packet for verification: {}", e]);
                    return Err(gst::FlowError::Error);
                }
            }

            gop_state.gop_started = false;
        } else {
            gst::warning!(CAT, imp = self, "Received verification metadata but no DSC manager available");
        }

        Ok(())
    }

    fn verify_signature(
        &self,
        dsc_verification: &gst_video::H274DigitallySignedContentVerification,
        data_packet: &[u8],
        hash_method: HashMethod,
        pkey: &RsaPublicKey,
        cert_status: CertVerificationStatus,
    ) -> Result<(), gst::FlowError> {
        let obj = self.obj();
        gst::debug!(CAT, imp = self, "Verifying substream data packet: {} bytes with hash method: {:?}", data_packet.len(), hash_method);
        let signature = dsc_verification.signature();
        gst::log!(CAT, imp = self, "Data packet content (first 32 bytes): {:02x?}", &data_packet[..std::cmp::min(32, data_packet.len())]);
        gst::log!(CAT, imp = self, "Signature content (hex): {:02x?}", &signature[..std::cmp::min(32, signature.len())]);
        gst::log!(CAT, imp = self, "Verifying signature ({} bytes) against substream data packet", signature.len());

        let fail_on_error = self.settings.lock().unwrap().fail_on_verification_error;
        match self.verify_rsa_pkcs1v15_signature(hash_method, pkey, data_packet, signature) {
            Ok(true) => {
                match cert_status {
                    CertVerificationStatus::Verified => {
                        gst::info!(CAT, imp = self, "\x1b[1;32m✅ Signature is valid.\x1b[0m");
                    }
                    CertVerificationStatus::Untrusted => {
                        gst::warning!(CAT, imp = self, "\x1b[1;31m⚠️ Signature is valid, but CA is untrusted.\x1b[0m");
                    }
                    CertVerificationStatus::Unverified => {
                        gst::info!(CAT, imp = self, "✅ Signature is valid (certificate not verified)");
                    }
                    CertVerificationStatus::Error => {
                        gst::warning!(CAT, imp = self, "✅ Signature is valid (certificate verification error)");
                    }
                }
                let s = gst::Structure::builder("dsc-verification-result")
                    .field("verified", true)
                    .field("certificate-trusted", cert_status == CertVerificationStatus::Verified)
                    .field("certificate-status", format!("{:?}", cert_status))
                    .field("data-packet-size", data_packet.len() as u64)
                    .field("signature-size", signature.len() as u64)
                    .build();
                let msg = gst::message::Element::new(s);
                let _ = obj.post_message(msg);

                if cert_status == CertVerificationStatus::Untrusted {
                    if fail_on_error {
                        gst::error!(CAT, imp = self, "Signature valid but certificate is untrusted");
                        return Err(gst::FlowError::Error);
                    } else {
                        gst::warning!(CAT, imp = self, "Signature valid but certificate is untrusted, continuing due to fail-on-verification-error=false");
                    }
                }

                Ok(())
            },
            Ok(false) => {
                gst::error!(CAT, imp = self, "❌ Substream signature verification FAILED");

                let s = gst::Structure::builder("dsc-verification-result")
                    .field("verified", false)
                    .field("data-packet-size", data_packet.len() as u64)
                    .field("signature-size", signature.len() as u64)
                    .field("error", "Signature verification failed")
                    .build();
                let msg = gst::message::Element::new(s);
                let _ = obj.post_message(msg);

                if fail_on_error {
                    gst::error!(CAT, imp = self, "Failing due to verification failure (fail-on-verification-error=true)");
                    Err(gst::FlowError::Error)
                } else {
                    gst::warning!(CAT, imp = self, "Continuing despite verification failure (fail-on-verification-error=false)");
                    Ok(())
                }
            },
            Err(e) => {
                gst::error!(CAT, imp = self, "Error during signature verification: {}", e);

                let s = gst::Structure::builder("dsc-verification-result")
                    .field("verified", false)
                    .field("data-packet-size", data_packet.len() as u64)
                    .field("signature-size", signature.len() as u64)
                    .field("error", format!("Verification error: {}", e))
                    .build();
                let msg = gst::message::Element::new(s);
                let _ = obj.post_message(msg);

                if fail_on_error {
                    gst::error!(CAT, imp = self, "Failing due to verification error (fail-on-verification-error=true)");
                    Err(gst::FlowError::Error)
                } else {
                    gst::warning!(CAT, imp = self, "Continuing despite verification error (fail-on-verification-error=false)");
                    Ok(())
                }
            }
        }
    }

    fn iterate_internal_links(&self, pad: &gst::Pad) -> gst::Iterator<gst::Pad> {
        let state = self.state.lock().unwrap();
        let otherpad = match pad.direction() {
            gst::PadDirection::Src => state.sinkpad.clone(),
            gst::PadDirection::Sink => state.srcpad.clone(),
            _ => unreachable!(),
        };
        if let Some(otherpad) = otherpad {
            gst::Iterator::from_vec(vec![otherpad])
        } else {
            gst::Iterator::from_vec(vec![])
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let buffer_mode = self.settings.lock().unwrap().buffer;

        let srcpad = {
            let state = self.state.lock().unwrap();
            state.srcpad.clone().ok_or(gst::FlowError::Error)?
        };

        let mut state = self.state.lock().unwrap();
        let gop_state = &mut state.gop_state;

        let initialization_meta = buffer.meta::<gst_video::video_meta::VideoDSCInitializationMeta>();
        let verification_meta = buffer.meta::<gst_video::video_meta::VideoDSCVerificationMeta>();
        let selection_meta = buffer.meta::<gst_video::video_meta::VideoDSCSelectionMeta>();

        let was_gop_started_before = gop_state.gop_started;

        if let Some(init_meta) = initialization_meta {
            self.handle_initialization_meta(&init_meta, gop_state)?;
        }

        // Capture whether the GOP was active BEFORE processing metas —
        // finalize_and_verify() resets gop_started, but we still need to drain.
        let gop_was_active = was_gop_started_before || gop_state.gop_started;

        let is_verification = verification_meta.is_some();

        if let Some(verif_meta) = verification_meta {
            self.handle_verification_meta_inner(&buffer, &verif_meta, selection_meta.as_deref(), gop_state)?;
        } else {
            self.handle_regular_buffer_inner(&buffer, selection_meta.as_deref(), gop_state)?;
        }

        if buffer_mode && gop_was_active {
            // Buffering mode: queue the buffer, do not output yet
            state.queued_buffers.push_back(buffer);

            if is_verification {
                // Verification completed (bus message already posted by verify_signature).
                // Measure GOP duration from timestamps for latency reporting.
                let gop_duration = {
                    let first_pts = state.queued_buffers.front()
                        .and_then(|b| b.pts());
                    let last_buf = state.queued_buffers.back().unwrap();
                    let last_end = last_buf.pts().opt_add(last_buf.duration());
                    match (first_pts, last_end) {
                        (Some(first), Some(last)) => last.opt_sub(first),
                        _ => None,
                    }
                };

                // Now drain all queued buffers: push them in order to srcpad.
                gst::debug!(CAT, imp = self, "Draining {} queued buffers after verification (GOP duration: {:?})",
                    state.queued_buffers.len(),
                    gop_duration);
                let buffers: Vec<_> = state.queued_buffers.drain(..).collect();
                drop(state);

                // Update measured latency and trigger pipeline renegotiation
                if let Some(duration) = gop_duration {
                    self.state.lock().unwrap().gop_latency = Some(duration);
                    self.post_message(gst::message::Latency::builder().src(&*self.obj()).build());
                }

                for buf in buffers {
                    srcpad.push(buf)?;
                }
            }

            Ok(gst::FlowSuccess::Ok)
        } else {
            // Passthrough mode or GOP not yet started: push immediately
            drop(state);
            srcpad.push(buffer)
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        let obj = self.obj();
        match event.view() {
            gst::EventView::Caps(caps) => {
                gst::debug!(CAT, imp = self, "Received caps: {}", caps.caps());
                if let Ok((codec, stream_format)) = VideoCodec::from_caps(caps.caps()) {
                    let mut state = self.state.lock().unwrap();
                    state.gop_state.nal_parser = Some(NalParser::new(codec, stream_format));
                    gst::info!(CAT, imp = self, "Initialized NAL parser for codec: {:?}, stream-format: {:?}", codec, stream_format);
                } else {
                    gst::warning!(CAT, imp = self, "Could not determine codec from caps");
                }
            }
            gst::EventView::FlushStop(_flush) => {
                gst::debug!(CAT, obj = obj, "flushing stored data");
                let mut state = self.state.lock().unwrap();
                state.gop_state = GopVerificationState {
                    nal_parser: state.gop_state.nal_parser.clone(),
                    ..Default::default()
                };
                state.queued_buffers.clear();
                state.gop_latency = None;
            }
            gst::EventView::Eos(_eos) => {
                // Drain any buffered data and forward EOS
                let mut state = self.state.lock().unwrap();
                state.queued_buffers.clear();
                state.gop_latency = None;
                drop(state);
            }
            _ => (),
        }

        gst::Pad::event_default(pad, Some(&*obj), event)
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        let obj = self.obj();
        match query.view_mut() {
            gst::QueryViewMut::Latency(latency) => {
                let mut upstream_query = gst::query::Latency::new();
                let otherpad = {
                    let state = self.state.lock().unwrap();
                    state.sinkpad.clone()
                };
                let Some(otherpad) = otherpad else {
                    return false;
                };
                let ret = otherpad.peer_query(&mut upstream_query);

                if ret {
                    let (live, mut min, mut max) = upstream_query.result();
                    let buffer_mode = self.settings.lock().unwrap().buffer;
                    if buffer_mode {
                        if let Some(gop_lat) = self.state.lock().unwrap().gop_latency {
                            min += gop_lat;
                            max = max.opt_add(gop_lat);
                        }
                        // If no measurement yet, first GOP passes through and latency
                        // will be updated when it drains (via post_message(Latency)).
                    }
                    latency.set(live, min, max);
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "Latency query response: live {} min {} max {} (buffer_mode={})",
                        live,
                        min,
                        max.display(),
                        buffer_mode
                    );
                }
                ret
            }
            _ => gst::Pad::query_default(pad, Some(&*obj), query),
        }
    }

    /// Internal: handles regular buffer (non-DSC metadata) for the chain path.
    /// Takes a buffer reference instead of borrowing the full buffer to allow
    /// the caller to own and push the buffer afterward.
    fn handle_regular_buffer_inner(
        &self,
        buffer: &gst::BufferRef,
        selection_meta: Option<&gst_video::video_meta::VideoDSCSelectionMeta>,
        gop_state: &mut GopVerificationState,
    ) -> Result<(), gst::FlowError> {
        if !gop_state.gop_started || gop_state.dsc_manager.is_none() {
            return Ok(());
        }

        let nal_units_to_hash = self.extract_nal_units(buffer, gop_state)?;
        let substream_id = self.get_substream_id(selection_meta, 0);

        self.add_nal_units_to_substream(&nal_units_to_hash, substream_id, gop_state, false)?;

        Ok(())
    }

    /// Internal: handles verification buffer for the chain path.
    fn handle_verification_meta_inner(
        &self,
        buffer: &gst::BufferRef,
        verif_meta: &gst_video::video_meta::VideoDSCVerificationMeta,
        selection_meta: Option<&gst_video::video_meta::VideoDSCSelectionMeta>,
        gop_state: &mut GopVerificationState,
    ) -> Result<(), gst::FlowError> {
        let dsc_verification = verif_meta.dsc_verification();

        gst::debug!(CAT, imp = self, "Found DSC verification metadata - verifying substream, signature length: {}",
            dsc_verification.signature().len());

        if !gop_state.gop_started {
            gst::warning!(CAT, imp = self, "Received verification metadata but no initialization metadata was received");
            return Ok(());
        }

        let nal_units_to_hash = self.extract_nal_units(buffer, gop_state)?;
        let substream_id = self.get_substream_id(selection_meta, dsc_verification.verification_substream_id() as usize);

        self.add_nal_units_to_substream(&nal_units_to_hash, substream_id, gop_state, true)?;
        self.finalize_and_verify(&dsc_verification, substream_id, gop_state)?;

        Ok(())
    }
}
