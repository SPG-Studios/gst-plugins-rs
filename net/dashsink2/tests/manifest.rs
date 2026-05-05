// Copyright (C) 2025 Roberto Viola <rviola@vicomtech.org>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gstdashsink2::dashsink2::manifest::{Manifest, ManifestType, MediaRepresentation};

fn video_rep(id: &str, duration: u32) -> MediaRepresentation {
    MediaRepresentation {
        id: id.to_string(),
        is_video: true,
        codec: "avc1.64001e".to_string(),
        width: Some(1280),
        height: Some(720),
        framerate: Some("30/1".to_string()),
        bandwidth: None,
        init_location: format!("{}_init.cmfi", id),
        segment_template: format!("{}_segment_$Number$.cmfv", id),
        segment_duration: duration,
    }
}

fn audio_rep(id: &str, duration: u32) -> MediaRepresentation {
    MediaRepresentation {
        id: id.to_string(),
        is_video: false,
        codec: "mp4a.40.2".to_string(),
        width: None,
        height: None,
        framerate: None,
        bandwidth: None,
        init_location: format!("{}_init.cmfi", id),
        segment_template: format!("{}_segment_$Number$.cmfa", id),
        segment_duration: duration,
    }
}

#[test]
fn test_new_manifest_defaults() {
    let m = Manifest::new();
    let xml = m.to_string().unwrap();

    assert!(xml.contains("type=\"static\""));
    assert!(xml.contains("minBufferTime=\"PT10S\""));
    assert!(xml.contains("<Period"));
    assert!(xml.contains("id=\"P0\""));
}

#[test]
fn test_set_mpd_type_dynamic() {
    let mut m = Manifest::new();
    m.set_mpd_type(ManifestType::Dynamic);
    let xml = m.to_string().unwrap();

    assert!(xml.contains("type=\"dynamic\""));
    assert!(xml.contains("urn:mpeg:dash:profile:isoff-live:2011"));
    assert!(xml.contains("minimumUpdatePeriod="));
    assert!(!xml.contains("mediaPresentationDuration="));
}

#[test]
fn test_set_mpd_type_static() {
    let mut m = Manifest::new();
    m.set_mpd_type(ManifestType::Dynamic);
    m.set_mpd_type(ManifestType::Static);
    let xml = m.to_string().unwrap();

    assert!(xml.contains("type=\"static\""));
    assert!(!xml.contains("minimumUpdatePeriod="));
    assert!(!xml.contains("availabilityStartTime="));
}

#[test]
fn test_add_video_representation() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 2000));
    let xml = m.to_string().unwrap();

    assert!(xml.contains("contentType=\"video\""));
    assert!(xml.contains("mimeType=\"video/mp4\""));
    assert!(xml.contains("id=\"video_0\""));
    assert!(xml.contains("codecs=\"avc1.64001e\""));
    assert!(xml.contains("width=\"1280\""));
    assert!(xml.contains("height=\"720\""));
    assert!(xml.contains("frameRate=\"30/1\""));
    assert!(xml.contains("timescale=\"1000\""));
    assert!(xml.contains("duration=\"2000\""));
    assert!(xml.contains("startNumber=\"0\""));
}

#[test]
fn test_add_audio_representation() {
    let mut m = Manifest::new();
    m.add_representation(audio_rep("audio_0", 2000));
    let xml = m.to_string().unwrap();

    assert!(xml.contains("contentType=\"audio\""));
    assert!(xml.contains("mimeType=\"audio/mp4\""));
    assert!(xml.contains("id=\"audio_0\""));
    assert!(xml.contains("codecs=\"mp4a.40.2\""));
    assert!(!xml.contains("width="));
    assert!(!xml.contains("frameRate="));
}

#[test]
fn test_add_video_and_audio() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 2000));
    m.add_representation(audio_rep("audio_0", 2000));
    let xml = m.to_string().unwrap();

    // Should have separate AdaptationSets
    assert!(xml.contains("contentType=\"video\""));
    assert!(xml.contains("contentType=\"audio\""));
    assert!(xml.contains("id=\"video_0\""));
    assert!(xml.contains("id=\"audio_0\""));
}

#[test]
fn test_add_multiple_video_representations() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 2000));
    m.add_representation(video_rep("video_1", 2000));
    let xml = m.to_string().unwrap();

    // Both reps should be under the same video AdaptationSet
    assert!(xml.contains("id=\"video_0\""));
    assert!(xml.contains("id=\"video_1\""));
    // Only one video AdaptationSet
    assert_eq!(xml.matches("contentType=\"video\"").count(), 1);
}

#[test]
fn test_add_segment_updates_bandwidth() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 2000));
    m.add_segment("video_0".to_string(), 1, 500_000);
    let xml = m.to_string().unwrap();

    assert!(xml.contains("bandwidth=\"500000\""));
}

#[test]
fn test_add_segment_updates_presentation_duration() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 2000));

    m.add_segment("video_0".to_string(), 1, 100_000);
    let xml = m.to_string().unwrap();
    assert!(xml.contains("mediaPresentationDuration=\"PT2S\""));

    m.add_segment("video_0".to_string(), 2, 100_000);
    let xml = m.to_string().unwrap();
    assert!(xml.contains("mediaPresentationDuration=\"PT4S\""));

    m.add_segment("video_0".to_string(), 3, 100_000);
    let xml = m.to_string().unwrap();
    assert!(xml.contains("mediaPresentationDuration=\"PT6S\""));
}

#[test]
fn test_add_segment_dynamic_sets_availability_and_publish() {
    let mut m = Manifest::new();
    m.set_mpd_type(ManifestType::Dynamic);
    m.add_representation(video_rep("video_0", 2000));
    m.add_segment("video_0".to_string(), 1, 100_000);
    let xml = m.to_string().unwrap();

    assert!(xml.contains("availabilityStartTime="));
    assert!(xml.contains("publishTime="));
}

#[test]
fn test_set_utc_timing_url_dynamic() {
    let mut m = Manifest::new();
    m.set_mpd_type(ManifestType::Dynamic);
    m.set_utc_timing_url("https://time.akamai.com/?iso".to_string());
    let xml = m.to_string().unwrap();

    assert!(xml.contains("UTCTiming"));
    assert!(xml.contains("urn:mpeg:dash:utc:http-xsdate:2014"));
    assert!(xml.contains("https://time.akamai.com/?iso"));
}

#[test]
fn test_set_utc_timing_url_not_in_static() {
    let mut m = Manifest::new();
    // Set in static mode — should store but not appear in XML
    m.set_utc_timing_url("https://time.example.com".to_string());
    let xml = m.to_string().unwrap();
    assert!(!xml.contains("UTCTiming"));
}

#[test]
fn test_xml_has_declaration() {
    let m = Manifest::new();
    let xml = m.to_string().unwrap();
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
}

#[test]
fn test_min_buffer_time_unit() {
    let mut m = Manifest::new();
    m.set_min_buffer_time(2000);
    let xml = m.to_string().unwrap();

    assert!(
        xml.contains("minBufferTime=\"PT2S\""),
        "2000ms should produce minBufferTime=PT2S, got: {}",
        xml
    );
}

#[test]
fn test_minimum_update_period_unit() {
    let mut m = Manifest::new();
    m.set_mpd_type(ManifestType::Dynamic);
    m.set_minimum_update_period(5000);
    let xml = m.to_string().unwrap();

    assert!(
        xml.contains("minimumUpdatePeriod=\"PT5S\""),
        "5000ms should produce minimumUpdatePeriod=PT5S, got: {}",
        xml
    );
}

#[test]
fn test_dynamic_to_static_finalization() {
    let mut m = Manifest::new();
    m.set_mpd_type(ManifestType::Dynamic);
    m.add_representation(video_rep("video_0", 2000));
    m.add_segment("video_0".to_string(), 1, 100_000);

    // Finalize to static (simulates PausedToReady transition)
    m.set_mpd_type(ManifestType::Static);
    let xml = m.to_string().unwrap();

    assert!(xml.contains("type=\"static\""));
    assert!(!xml.contains("minimumUpdatePeriod="));
    assert!(!xml.contains("availabilityStartTime="));
    assert!(!xml.contains("publishTime="));
    // After live→VOD transition, mediaPresentationDuration should exist
    assert!(xml.contains("mediaPresentationDuration="));
}

#[test]
fn test_segment_template_contents() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 4000));
    let xml = m.to_string().unwrap();

    assert!(xml.contains("duration=\"4000\""));
    assert!(xml.contains("timescale=\"1000\""));
    assert!(xml.contains("startNumber=\"0\""));
    assert!(xml.contains("initialization=\"video_0_init.cmfi\""));
    assert!(xml.contains("media=\"video_0_segment_$Number$.cmfv\""));
}

#[test]
fn test_add_segment_unknown_rep_is_noop() {
    let mut m = Manifest::new();
    m.add_representation(video_rep("video_0", 2000));
    // Adding a segment for a non-existent representation should not panic
    m.add_segment("video_99".to_string(), 1, 100_000);
    let xml = m.to_string().unwrap();
    // bandwidth should not appear since the rep was not found
    assert!(!xml.contains("bandwidth=\"100000\""));
}
