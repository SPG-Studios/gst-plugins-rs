// SPDX-License-Identifier: MPL-2.0

//! Filtering of ICE candidates as they pass through the signalling server.
//!
//! The signalling server itself never generates ICE candidates, those are
//! produced by each peer's `webrtcbin`/libnice instance. Still, when the
//! deployment requires that peers only use a constrained UDP port range
//! (e.g. to traverse a firewall) or refuses TCP entirely, candidates that
//! do not match those constraints can be dropped here before they reach
//! the remote peer.
//!
//! Two pieces of state need to be inspected:
//!
//! * Standalone trickle ICE candidate messages (`PeerMessageInner::Ice`).
//! * `a=candidate:` lines embedded inside SDP offers and answers.

use tracing::debug;

/// Constraints applied to ICE candidates forwarded by the signalling server.
///
/// A default-constructed value performs no filtering and lets every
/// candidate through, preserving the historical behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IceFilterConfig {
    /// Lowest UDP port (inclusive) allowed in a forwarded ICE candidate.
    ///
    /// Candidates whose port is strictly below this value are dropped.
    pub min_port: Option<u16>,
    /// Highest UDP port (inclusive) allowed in a forwarded ICE candidate.
    ///
    /// Candidates whose port is strictly above this value are dropped.
    pub max_port: Option<u16>,
    /// When `true`, candidates using a non-UDP transport (typically TCP)
    /// are dropped unconditionally.
    pub udp_only: bool,
}

impl IceFilterConfig {
    /// Returns `true` when the configuration would actually filter
    /// anything. This lets callers skip work for the common no-op case.
    pub fn is_active(&self) -> bool {
        self.udp_only || self.min_port.is_some() || self.max_port.is_some()
    }

    /// Validate the configuration, returning an error message when it is
    /// internally inconsistent.
    pub fn validate(&self) -> Result<(), String> {
        if let (Some(min), Some(max)) = (self.min_port, self.max_port)
            && min > max
        {
            return Err(format!(
                "min-rtp-port ({min}) must be lower than or equal to max-rtp-port ({max})"
            ));
        }
        Ok(())
    }
}

/// Decide whether a single ICE candidate string passes the filter.
///
/// The candidate is expected to follow the syntax described in
/// RFC 5245 §15.1, optionally prefixed with `a=` as it appears in SDP:
///
/// ```text
/// candidate:<foundation> <component-id> <transport> <priority>
///           <connection-address> <port> typ <cand-type> ...
/// ```
///
/// Strings that do not match that shape (for instance an empty
/// end-of-candidates marker) are left untouched and forwarded as-is.
pub fn candidate_passes_filter(candidate: &str, config: &IceFilterConfig) -> bool {
    if !config.is_active() {
        return true;
    }

    let trimmed = candidate.trim();
    let trimmed = trimmed.strip_prefix("a=").unwrap_or(trimmed);

    let Some(body) = trimmed.strip_prefix("candidate:") else {
        return true;
    };

    // After "candidate:" the fields are, in order:
    //   foundation component-id transport priority connection-address port typ cand-type ...
    let parts: Vec<&str> = body.split_ascii_whitespace().collect();
    if parts.len() < 8 {
        return true;
    }

    let transport = parts[2];
    if config.udp_only && !transport.eq_ignore_ascii_case("UDP") {
        return false;
    }

    let Ok(port) = parts[5].parse::<u16>() else {
        return true;
    };

    if let Some(min) = config.min_port
        && port < min
    {
        return false;
    }
    if let Some(max) = config.max_port
        && port > max
    {
        return false;
    }

    true
}

/// Filter every `a=candidate:` line inside an SDP blob, dropping the
/// ones that do not match the configured constraints. Non-candidate
/// lines are preserved verbatim, including the original line endings.
pub fn filter_sdp(sdp: &str, config: &IceFilterConfig) -> String {
    if !config.is_active() {
        return sdp.to_string();
    }

    let mut result = String::with_capacity(sdp.len());
    for line_with_ending in sdp.split_inclusive('\n') {
        let line = line_with_ending
            .trim_end_matches('\n')
            .trim_end_matches('\r');

        if line.starts_with("a=candidate:") && !candidate_passes_filter(line, config) {
            debug!("Dropping ICE candidate from SDP: {line}");
            continue;
        }

        result.push_str(line_with_ending);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(min: Option<u16>, max: Option<u16>, udp_only: bool) -> IceFilterConfig {
        IceFilterConfig {
            min_port: min,
            max_port: max,
            udp_only,
        }
    }

    #[test]
    fn default_config_is_inactive() {
        let cfg = IceFilterConfig::default();
        assert!(!cfg.is_active());
        assert!(candidate_passes_filter(
            "candidate:1 1 TCP 1 192.0.2.1 9 typ host tcptype active",
            &cfg,
        ));
    }

    #[test]
    fn validate_rejects_inverted_range() {
        let inverted = cfg(Some(40000), Some(30000), false);
        assert!(inverted.validate().is_err());
        let ordered = cfg(Some(30000), Some(40000), false);
        assert!(ordered.validate().is_ok());
    }

    #[test]
    fn udp_only_drops_tcp_candidate() {
        let cfg = cfg(None, None, true);
        let udp = "candidate:1 1 UDP 2122194687 192.168.1.1 56789 typ host";
        let tcp = "candidate:2 1 TCP 1518214399 192.168.1.1 9 typ host tcptype active";
        assert!(candidate_passes_filter(udp, &cfg));
        assert!(!candidate_passes_filter(tcp, &cfg));
    }

    #[test]
    fn port_range_is_inclusive() {
        let cfg = cfg(Some(40000), Some(40100), true);
        assert!(candidate_passes_filter(
            "candidate:1 1 UDP 1 192.0.2.1 40000 typ host",
            &cfg
        ));
        assert!(candidate_passes_filter(
            "candidate:1 1 UDP 1 192.0.2.1 40100 typ host",
            &cfg
        ));
        assert!(!candidate_passes_filter(
            "candidate:1 1 UDP 1 192.0.2.1 39999 typ host",
            &cfg
        ));
        assert!(!candidate_passes_filter(
            "candidate:1 1 UDP 1 192.0.2.1 40101 typ host",
            &cfg
        ));
    }

    #[test]
    fn accepts_a_prefix_form() {
        let cfg = cfg(Some(40000), Some(40100), true);
        assert!(candidate_passes_filter(
            "a=candidate:1 1 UDP 1 192.0.2.1 40050 typ host",
            &cfg
        ));
        assert!(!candidate_passes_filter(
            "a=candidate:1 1 TCP 1 192.0.2.1 40050 typ host tcptype active",
            &cfg
        ));
    }

    #[test]
    fn non_candidate_strings_pass_through() {
        let cfg = cfg(Some(40000), Some(40100), true);
        assert!(candidate_passes_filter("", &cfg));
        assert!(candidate_passes_filter("end-of-candidates", &cfg));
        assert!(candidate_passes_filter("candidate", &cfg));
    }

    #[test]
    fn malformed_candidate_is_kept() {
        let cfg = cfg(Some(40000), Some(40100), true);
        assert!(candidate_passes_filter("candidate:1 1 UDP", &cfg));
    }

    #[test]
    fn filter_sdp_drops_matching_lines_and_keeps_endings() {
        let sdp = "v=0\r\n\
                   m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
                   a=candidate:1 1 UDP 2122194687 192.0.2.1 40050 typ host\r\n\
                   a=candidate:2 1 TCP 1518214399 192.0.2.1 9 typ host tcptype active\r\n\
                   a=candidate:3 1 UDP 1 192.0.2.1 1234 typ host\r\n\
                   a=end-of-candidates\r\n";
        let cfg = cfg(Some(40000), Some(40100), true);
        let filtered = filter_sdp(sdp, &cfg);
        assert!(filtered.contains("a=candidate:1 1 UDP"));
        assert!(!filtered.contains(" TCP "));
        assert!(!filtered.contains("192.0.2.1 1234"));
        assert!(filtered.contains("a=end-of-candidates"));
        assert!(filtered.contains("v=0\r\n"));
        assert!(filtered.ends_with("\r\n"));
    }

    #[test]
    fn filter_sdp_no_op_when_inactive() {
        let sdp = "v=0\n\
                   a=candidate:1 1 TCP 1 192.0.2.1 1 typ host tcptype active\n";
        let cfg = IceFilterConfig::default();
        assert_eq!(filter_sdp(sdp, &cfg), sdp);
    }
}
