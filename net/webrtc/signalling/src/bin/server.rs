// SPDX-License-Identifier: MPL-2.0

use anyhow::{Error, bail};
use clap::Parser;
use gst_plugin_webrtc_signalling::handlers::{Handler, IceFilterConfig};
use gst_plugin_webrtc_signalling::server::{Server, ServerError};
use std::net::IpAddr;
use std::time::Duration;
use tokio::{net::TcpListener, net::TcpStream, task};
use tracing::{info, warn};
use tracing_subscriber::prelude::*;

use std::{path::PathBuf, sync::Arc};
use tokio_rustls::{TlsAcceptor, rustls};

const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[clap(about, version, author)]
/// Program arguments
struct Args {
    /// Address to listen on
    #[clap(long, default_value = "0.0.0.0")]
    host: String,
    /// Port to listen on
    #[clap(short, long, default_value_t = 8443)]
    port: u16,
    /// TLS certificate to use
    #[clap(short, long)]
    cert: Option<String>,
    /// Private key to use
    #[clap(short, long)]
    key: Option<String>,
    /// Drop forwarded ICE candidates whose UDP port is below this value
    /// (inclusive bound).
    #[clap(long)]
    min_rtp_port: Option<u16>,
    /// Drop forwarded ICE candidates whose UDP port is above this value
    /// (inclusive bound).
    #[clap(long)]
    max_rtp_port: Option<u16>,
    /// Drop forwarded ICE candidates that do not use UDP as transport
    /// (i.e. TCP host/relay candidates).
    #[clap(long, default_value_t = false)]
    udp_only: bool,
    /// Enable NAT / multi-homed traversal: occurrences of this internal IP in
    /// outgoing ICE candidates and SDP payloads are rewritten to the local
    /// socket address each peer connected to. Useful when the producer
    /// advertises a private address (e.g. behind a NAT, or on a different
    /// subnet) that consumers cannot route to.
    #[clap(
        long = "multi-homed-traversal",
        alias = "rewrite-ice-ip",
        value_name = "INTERNAL_IP"
    )]
    multi_homed_traversal: Option<IpAddr>,
}

/// Best-effort local IP of a freshly-accepted TCP connection. Returns the
/// IPv4 form for IPv4-mapped IPv6 addresses so the string matches plain
/// IPv4 ICE candidates.
fn local_ip_of(stream: &TcpStream) -> Option<IpAddr> {
    let ip = stream.local_addr().ok()?.ip();
    Some(match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    })
}

fn initialize_logging(envvar_name: &str) -> Result<(), Error> {
    tracing_log::LogTracer::init()?;
    let env_filter = tracing_subscriber::EnvFilter::try_from_env(envvar_name)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_target(true)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        );
    let subscriber = tracing_subscriber::Registry::default()
        .with(env_filter)
        .with(fmt_layer);
    tracing::subscriber::set_global_default(subscriber)?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();

    let ice_filter = IceFilterConfig {
        min_port: args.min_rtp_port,
        max_port: args.max_rtp_port,
        udp_only: args.udp_only,
    };
    if let Err(err) = ice_filter.validate() {
        bail!(err);
    }

    initialize_logging("WEBRTCSINK_SIGNALLING_SERVER_LOG")?;

    if ice_filter.is_active() {
        info!(
            "ICE candidate filtering enabled: udp_only={}, min_rtp_port={:?}, max_rtp_port={:?}",
            ice_filter.udp_only, ice_filter.min_port, ice_filter.max_port,
        );
    }
    if let Some(ip) = args.multi_homed_traversal {
        info!(
            "Multi-homed traversal enabled: rewriting {} in outgoing ICE/SDP to each peer's local socket IP",
            ip
        );
    }

    let server = Server::spawn(move |stream| Handler::with_filter(stream, ice_filter))
        .with_rewrite_ip(args.multi_homed_traversal);

    let addr = format!("{}:{}", args.host, args.port);

    // Create the event loop and TCP listener we'll accept connections on.
    let listener = TcpListener::bind(&addr).await?;

    let acceptor = if let (Some(cert), Some(key)) = (&args.cert, &args.key) {
        create_tls_acceptor(cert, key).await.ok()
    } else {
        None
    };

    info!("Listening on: {}", addr);

    while let Ok((stream, address)) = listener.accept().await {
        let mut server_clone = server.clone();
        let local_ip = local_ip_of(&stream);
        info!(
            "Accepting connection from {} (local {:?})",
            address, local_ip
        );

        match acceptor.clone() {
            Some(acceptor) => {
                tokio::spawn(async move {
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        Ok(Ok(stream)) => {
                            server_clone
                                .accept_async_with_local_addr(stream, local_ip)
                                .await
                        }
                        Ok(Err(err)) => {
                            warn!("Failed to accept TLS connection from {}: {}", address, err);
                            Err(ServerError::TLSHandshake(err))
                        }
                        Err(elapsed) => {
                            warn!("TLS connection timed out {} after {}", address, elapsed);
                            Err(ServerError::TLSHandshakeTimeout(elapsed))
                        }
                    }
                });
            }
            _ => {
                task::spawn(async move {
                    server_clone
                        .accept_async_with_local_addr(stream, local_ip)
                        .await
                });
            }
        }
    }

    Ok(())
}

fn read_certs_from_file(
    certificate_file: PathBuf,
) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, Box<dyn std::error::Error>> {
    use rustls_pki_types::pem::PemObject;

    let certs_iter = rustls_pki_types::CertificateDer::pem_file_iter(&certificate_file)?;
    let mut certs = Vec::new();
    for cert_result in certs_iter {
        match cert_result {
            Ok(cert) => certs.push(cert),
            Err(e) => {
                return Err(format!("Failed to parse certificate: {e}").into());
            }
        }
    }

    if certs.is_empty() {
        return Err(format!(
            "No valid certificates found in {}",
            certificate_file.display()
        )
        .into());
    }

    Ok(certs)
}

fn read_private_key_from_file(
    private_key_file: PathBuf,
) -> Result<rustls_pki_types::PrivateKeyDer<'static>, Box<dyn std::error::Error>> {
    use rustls_pki_types::pem::PemObject;

    Ok(rustls_pki_types::PrivateKeyDer::from_pem_file(
        &private_key_file,
    )?)
}

pub async fn create_tls_acceptor(
    certificate_file: &str,
    private_key_file: &str,
) -> Result<TlsAcceptor, Box<dyn std::error::Error>> {
    let ring_provider = rustls::crypto::ring::default_provider();
    let certs = read_certs_from_file(certificate_file.into())?;
    let key = read_private_key_from_file(private_key_file.into())?;

    let config = rustls::ServerConfig::builder_with_provider(ring_provider.into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}
