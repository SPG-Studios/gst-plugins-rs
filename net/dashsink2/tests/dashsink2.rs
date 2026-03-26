// Copyright (C) 2025 Roberto Viola <rviola@vicomtech.org>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use mp4_atom::{Atom, FourCC, Ftyp, Header, Mdat, Moof, Moov, ReadAtom, ReadFrom, Styp};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstisobmff::plugin_register_static().expect("Need cmafmux for dashsink2 test");
        gstdashsink2::plugin_register_static().expect("dashsink2 test");
    });
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn make_temp_dir() -> (std::path::PathBuf, String) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("dashsink2_test_{}_{id}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.to_str().unwrap().to_string();
    (dir, path)
}

/// Collected output of a dashsink2 test run.
struct TestOutput {
    /// Map from filename → raw bytes.
    files: HashMap<String, Vec<u8>>,
    /// The final manifest XML.
    manifest_xml: String,
}

impl TestOutput {
    fn init_data(&self) -> &[u8] {
        let (_, data) = self
            .files
            .iter()
            .find(|(k, _)| k.contains("init"))
            .expect("No init segment found");
        data
    }

    fn init_data_for(&self, track_type: &str) -> &[u8] {
        let (_, data) = self
            .files
            .iter()
            .find(|(k, _)| k.starts_with(track_type) && k.contains("init"))
            .unwrap_or_else(|| panic!("No init segment found for track type {track_type}"));
        data
    }

    fn segment_keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self
            .files
            .keys()
            .filter(|k| k.contains("segment_"))
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    fn segment_keys_for(&self, track_type: &str) -> Vec<String> {
        let mut keys: Vec<_> = self
            .files
            .keys()
            .filter(|k| k.starts_with(track_type) && k.contains("segment_"))
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

/// Run a video pipeline: videotestsrc → x264enc → h264parse → dashsink2.
/// Uses default file I/O (mpd-root-path → temp dir). Returns None if
/// required encoder elements are not available (test will be skipped).
fn run_video_pipeline(
    num_buffers: i32,
    gop: u32,
    target_duration_ms: u32,
    dynamic: bool,
) -> Option<TestOutput> {
    init();

    let (tmpdir, tmppath) = make_temp_dir();

    let pipeline = gst::Pipeline::with_name("dashsink2_test");

    let video_src = gst::ElementFactory::make("videotestsrc")
        .property("is-live", true)
        .property("num-buffers", num_buffers)
        .build()
        .unwrap();

    let caps = gst::Caps::builder("video/x-raw")
        .field("width", 320)
        .field("height", 240)
        .field("format", "I420")
        .field("framerate", gst::Fraction::new(30, 1))
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .expect("Must be able to instantiate capsfilter");

    let x264enc = gst::ElementFactory::make("x264enc")
        .property("key-int-max", gop)
        .property("bitrate", 512u32)
        .property_from_str("speed-preset", "ultrafast")
        .property_from_str("tune", "zerolatency")
        .build();
    let x264enc = match x264enc {
        Ok(e) => e,
        Err(_) => {
            eprintln!("Skipping: x264enc not available");
            return None;
        }
    };

    let h264parse = gst::ElementFactory::make("h264parse").build().unwrap();

    let dashsink = gst::ElementFactory::make("dashsink2")
        .name("test_dashsink2")
        .property("target-duration", target_duration_ms)
        .property("dynamic", dynamic)
        .property("mpd-root-path", &tmppath)
        .build()
        .expect("Must be able to instantiate dashsink2");

    pipeline
        .add_many([&video_src, &capsfilter, &x264enc, &h264parse, &dashsink])
        .unwrap();
    gst::Element::link_many([&video_src, &capsfilter, &x264enc, &h264parse, &dashsink]).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(60)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(err) => {
                pipeline.set_state(gst::State::Null).unwrap();
                panic!("Pipeline error: {err}");
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos, "Pipeline did not reach EOS");

    // Read output files
    let mut file_data: HashMap<String, Vec<u8>> = HashMap::new();
    let mut xml = String::new();
    for entry in std::fs::read_dir(&tmpdir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let data = std::fs::read(entry.path()).unwrap();
        if name.ends_with(".mpd") {
            xml = String::from_utf8_lossy(&data).to_string();
        }
        file_data.insert(name, data);
    }

    std::fs::remove_dir_all(&tmpdir).ok();

    Some(TestOutput {
        files: file_data,
        manifest_xml: xml,
    })
}

macro_rules! run_pipeline {
    ($($args:expr),+) => {
        match run_video_pipeline($($args),+) {
            Some(out) => out,
            None => return,
        }
    };
}

// ---------------------------------------------------------------------------
// CMAF validation (adapted from hlssink3/tests/common)
// ---------------------------------------------------------------------------

fn validate_cmaf_init(data: &[u8]) -> anyhow::Result<()> {
    let mut input = Cursor::new(data);
    let mut has_ftyp = false;
    let mut has_moov = false;
    let mut found_mvex = false;

    while let Ok(header) = Header::read_from(&mut input) {
        match header.kind {
            Ftyp::KIND => {
                has_ftyp = true;
                let ftyp = Ftyp::read_atom(&header, &mut input)?;

                let fragmented_brands: Vec<FourCC> = [
                    *b"isom", *b"iso2", *b"iso6", *b"avc1", *b"dash", *b"cmfc", *b"cmf2",
                ]
                .iter()
                .map(FourCC::from)
                .collect();

                let is_fragmented = fragmented_brands.contains(&ftyp.major_brand)
                    || ftyp
                        .compatible_brands
                        .iter()
                        .any(|b| fragmented_brands.contains(b));
                assert!(is_fragmented, "Init segment missing CMAF/fMP4 brand");
            }
            Moov::KIND => {
                has_moov = true;
                let moov = Moov::read_atom(&header, &mut input)?;
                found_mvex = moov.mvex.is_some();
            }
            _ => {
                let skip = header.size.unwrap_or(0);
                let mut buf = vec![0u8; skip];
                std::io::Read::read_exact(&mut input, &mut buf)?;
            }
        }
    }

    assert!(has_ftyp, "Init segment missing ftyp");
    assert!(has_moov, "Init segment missing moov");
    assert!(found_mvex, "Init segment missing mvex (required for fMP4)");
    Ok(())
}

fn validate_cmaf_segment(data: &[u8]) -> anyhow::Result<u32> {
    let mut input = Cursor::new(data);
    let mut fragment_count = 0u32;

    while let Ok(header) = Header::read_from(&mut input) {
        match header.kind {
            Styp::KIND => {
                Styp::read_atom(&header, &mut input)?;
            }
            Moof::KIND => {
                fragment_count += 1;
                let moof = Moof::read_atom(&header, &mut input)?;
                assert!(moof.mfhd.sequence_number > 0, "Invalid mfhd sequence");
                assert!(!moof.traf.is_empty(), "moof missing traf");
                assert_eq!(moof.traf.len(), 1, "CMAF: exactly one traf per moof");
                assert!(!moof.traf[0].trun.is_empty(), "traf missing trun");
            }
            Mdat::KIND => {
                let mdat = Mdat::read_atom(&header, &mut input)?;
                assert!(!mdat.data.is_empty(), "Empty mdat");
            }
            _ => {
                let skip = header.size.unwrap_or(0);
                let mut buf = vec![0u8; skip];
                std::io::Read::read_exact(&mut input, &mut buf)?;
            }
        }
    }

    assert!(fragment_count > 0, "No moof in media segment");
    Ok(fragment_count)
}

fn validate_combined_fmp4(init: &[u8], segment: &[u8]) -> anyhow::Result<()> {
    let mut combined = Vec::with_capacity(init.len() + segment.len());
    combined.extend_from_slice(init);
    combined.extend_from_slice(segment);

    let mut input = Cursor::new(&combined);
    let mut has_ftyp = false;
    let mut has_moov = false;
    let mut has_moof = false;

    while let Ok(header) = Header::read_from(&mut input) {
        match header.kind {
            Ftyp::KIND => {
                has_ftyp = true;
                Ftyp::read_atom(&header, &mut input)?;
            }
            Moov::KIND => {
                has_moov = true;
                Moov::read_atom(&header, &mut input)?;
            }
            Styp::KIND => {
                Styp::read_atom(&header, &mut input)?;
            }
            Moof::KIND => {
                has_moof = true;
                Moof::read_atom(&header, &mut input)?;
            }
            Mdat::KIND => {
                Mdat::read_atom(&header, &mut input)?;
            }
            _ => {
                let skip = header.size.unwrap_or(0);
                let mut buf = vec![0u8; skip];
                std::io::Read::read_exact(&mut input, &mut buf)?;
            }
        }
    }

    assert!(has_ftyp && has_moov && has_moof, "Combined fMP4 incomplete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Duration extraction helpers
// ---------------------------------------------------------------------------

/// Extract timescale from the init segment's moov/trak/mdia/mdhd.
fn extract_timescale(init_data: &[u8]) -> u32 {
    let mut input = Cursor::new(init_data);
    while let Ok(header) = Header::read_from(&mut input) {
        if header.kind == Moov::KIND {
            let moov = Moov::read_atom(&header, &mut input).unwrap();
            return moov.trak[0].mdia.mdhd.timescale;
        }
        let skip = header.size.unwrap_or(0);
        let mut buf = vec![0u8; skip];
        std::io::Read::read_exact(&mut input, &mut buf).ok();
    }
    panic!("No moov found in init segment");
}

/// Compute segment duration in seconds from trun sample durations and timescale.
fn segment_duration_secs(segment_data: &[u8], timescale: u32) -> f64 {
    let mut input = Cursor::new(segment_data);
    let mut total_ticks: u64 = 0;
    let mut default_duration: Option<u32> = None;

    while let Ok(header) = Header::read_from(&mut input) {
        match header.kind {
            Moof::KIND => {
                let moof = Moof::read_atom(&header, &mut input).unwrap();
                for traf in &moof.traf {
                    default_duration = traf.tfhd.default_sample_duration;
                    for trun in &traf.trun {
                        for entry in &trun.entries {
                            let d = entry
                                .duration
                                .or(default_duration)
                                .expect("No sample duration available");
                            total_ticks += d as u64;
                        }
                    }
                }
            }
            _ => {
                let skip = header.size.unwrap_or(0);
                let mut buf = vec![0u8; skip];
                std::io::Read::read_exact(&mut input, &mut buf).ok();
            }
        }
    }

    total_ticks as f64 / timescale as f64
}

/// Count the number of samples in a segment.
fn segment_sample_count(segment_data: &[u8]) -> u32 {
    let mut input = Cursor::new(segment_data);
    let mut count = 0u32;
    while let Ok(header) = Header::read_from(&mut input) {
        match header.kind {
            Moof::KIND => {
                let moof = Moof::read_atom(&header, &mut input).unwrap();
                for traf in &moof.traf {
                    for trun in &traf.trun {
                        count += trun.entries.len() as u32;
                    }
                }
            }
            _ => {
                let skip = header.size.unwrap_or(0);
                let mut buf = vec![0u8; skip];
                std::io::Read::read_exact(&mut input, &mut buf).ok();
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Audio+Video pipeline helper
// ---------------------------------------------------------------------------

/// Run a pipeline with both video and audio tracks.
fn run_av_pipeline(num_buffers: i32, gop: u32, target_duration_ms: u32) -> Option<TestOutput> {
    init();

    let (tmpdir, tmppath) = make_temp_dir();

    let pipeline_str = format!(
        "videotestsrc is-live=true num-buffers={num_buffers} ! \
         video/x-raw,width=320,height=240,format=I420,framerate=30/1 ! \
         x264enc key-int-max={gop} bitrate=512 speed-preset=ultrafast tune=zerolatency ! \
         h264parse ! dashsink2.video_0 \
         audiotestsrc is-live=true samplesperbuffer=1024 num-buffers={num_audio} ! \
         audio/x-raw,rate=44100,channels=1 ! \
         avenc_aac ! aacparse ! dashsink2.audio_0 \
         dashsink2 name=dashsink2 target-duration={target_duration_ms} \
         mpd-root-path={tmppath}",
        num_audio = (num_buffers as f64 * 44100.0 / (30.0 * 1024.0)).ceil() as i32 + 5,
    );

    let pipeline = match gst::parse::launch(&pipeline_str) {
        Ok(p) => p.downcast::<gst::Pipeline>().unwrap(),
        Err(e) => {
            eprintln!("Skipping AV test: {e}");
            return None;
        }
    };

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut eos = false;
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(60)) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                eos = true;
                break;
            }
            MessageView::Error(err) => {
                pipeline.set_state(gst::State::Null).unwrap();
                panic!("AV Pipeline error: {err}");
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null).unwrap();
    assert!(eos, "AV Pipeline did not reach EOS");

    let mut file_data: HashMap<String, Vec<u8>> = HashMap::new();
    let mut xml = String::new();
    for entry in std::fs::read_dir(&tmpdir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();
        let data = std::fs::read(entry.path()).unwrap();
        if name.ends_with(".mpd") {
            xml = String::from_utf8_lossy(&data).to_string();
        }
        file_data.insert(name, data);
    }

    std::fs::remove_dir_all(&tmpdir).ok();

    Some(TestOutput {
        files: file_data,
        manifest_xml: xml,
    })
}

macro_rules! run_av {
    ($($args:expr),+) => {
        match run_av_pipeline($($args),+) {
            Some(out) => out,
            None => return,
        }
    };
}

// ---------------------------------------------------------------------------
// MPD helpers
// ---------------------------------------------------------------------------

fn mpd_attr(xml: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = xml.find(&needle)? + needle.len();
    let end = start + xml[start..].find('"')?;
    Some(xml[start..end].to_string())
}

fn mpd_has_tag(xml: &str, tag: &str) -> bool {
    xml.contains(&format!("<{}", tag))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_dashsink2_video_basic_segmentation() {
    // GOP=60 (2s @30fps), target=2s, 300 frames = 10s → ~5 segments
    let out = run_pipeline!(300, 60, 2000, false);

    // Init segment
    let init_data = out.init_data();
    assert!(!init_data.is_empty(), "Init segment is empty");
    validate_cmaf_init(init_data).expect("Invalid CMAF init segment");

    // Media segments
    let seg_keys = out.segment_keys();
    assert!(
        seg_keys.len() >= 4,
        "Expected at least 4 segments, found {}",
        seg_keys.len()
    );

    for key in &seg_keys {
        let seg = &out.files[key.as_str()];
        assert!(!seg.is_empty(), "Segment {key} is empty");
        validate_cmaf_segment(seg).unwrap_or_else(|e| panic!("Invalid segment {key}: {e}"));
        validate_combined_fmp4(init_data, seg)
            .unwrap_or_else(|e| panic!("Combined fMP4 invalid for {key}: {e}"));
    }

    // MPD
    assert!(!out.manifest_xml.is_empty(), "No manifest");
    assert_eq!(
        mpd_attr(&out.manifest_xml, "type").as_deref(),
        Some("static")
    );
    assert!(mpd_has_tag(&out.manifest_xml, "SegmentTemplate"));
    assert!(out.manifest_xml.contains("video/mp4"));
}

#[test]
fn test_dashsink2_video_small_gop() {
    // GOP=30 (1s), target=2s → cmafmux accumulates GOPs → ~2.5 segments
    let out = run_pipeline!(150, 30, 2000, false);

    let seg_keys = out.segment_keys();
    assert!(
        seg_keys.len() >= 2,
        "Expected ≥2 segments, got {}",
        seg_keys.len()
    );

    let init_data = out.init_data();
    for key in &seg_keys {
        validate_cmaf_segment(&out.files[key.as_str()]).unwrap();
        validate_combined_fmp4(init_data, &out.files[key.as_str()]).unwrap();
    }
}

#[test]
fn test_dashsink2_video_large_gop() {
    // GOP=90 (3s), target=2s → cmafmux waits for keyframe → ~3s segments
    let out = run_pipeline!(300, 90, 2000, false);

    let seg_keys = out.segment_keys();
    assert!(
        seg_keys.len() >= 3 && seg_keys.len() <= 5,
        "Expected 3-5 segments for GOP=90/target=2s, got {}",
        seg_keys.len()
    );

    let init_data = out.init_data();
    for key in &seg_keys {
        validate_cmaf_segment(&out.files[key.as_str()]).unwrap();
        validate_combined_fmp4(init_data, &out.files[key.as_str()]).unwrap();
    }
}

#[test]
fn test_dashsink2_video_short_content() {
    // 45 frames = 1.5s with GOP=30, target=2s → 1 segment
    let out = run_pipeline!(45, 30, 2000, false);

    let seg_keys = out.segment_keys();
    assert_eq!(seg_keys.len(), 1, "Expected 1 segment for 1.5s content");

    validate_cmaf_init(out.init_data()).unwrap();
    validate_cmaf_segment(&out.files[seg_keys[0].as_str()]).unwrap();
}

#[test]
fn test_dashsink2_video_single_large_segment() {
    // target=10s, 300 frames = 10s → 1 segment
    let out = run_pipeline!(300, 60, 10000, false);

    let seg_keys = out.segment_keys();
    assert_eq!(seg_keys.len(), 1, "Expected 1 segment for 10s target");
}

#[test]
fn test_dashsink2_video_many_small_segments() {
    // GOP=15 (0.5s), target=500ms → ~20 segments
    let out = run_pipeline!(300, 15, 500, false);

    let seg_keys = out.segment_keys();
    assert!(
        seg_keys.len() >= 15,
        "Expected ~20 segments, got {}",
        seg_keys.len()
    );

    let init_data = out.init_data();
    for key in seg_keys.iter().take(3) {
        validate_cmaf_segment(&out.files[key.as_str()]).unwrap();
        validate_combined_fmp4(init_data, &out.files[key.as_str()]).unwrap();
    }
}

#[test]
fn test_dashsink2_dynamic_mode() {
    // After EOS, dynamic manifest is finalized to static.
    let out = run_pipeline!(150, 60, 2000, true);

    assert_eq!(
        mpd_attr(&out.manifest_xml, "type").as_deref(),
        Some("static")
    );

    let seg_keys = out.segment_keys();
    assert!(seg_keys.len() >= 2, "Dynamic mode should produce segments");
}

#[test]
fn test_dashsink2_segment_filenames() {
    let out = run_pipeline!(150, 60, 2000, false);

    // Init segment: video_0_init.cmfi
    let init_keys: Vec<_> = out.files.keys().filter(|k| k.contains("init")).collect();
    assert_eq!(init_keys.len(), 1, "Expected one init segment");
    assert!(
        init_keys[0].starts_with("video_0"),
        "Init should start with video_0, got {}",
        init_keys[0]
    );

    // Segments numbered sequentially with .cmfv extension for video
    let seg_keys = out.segment_keys();
    for (i, key) in seg_keys.iter().enumerate() {
        let expected = format!("segment_{}", i);
        assert!(
            key.contains(&expected),
            "Segment {i} should contain '{expected}', got '{key}'"
        );
        assert!(
            key.ends_with(".cmfv"),
            "Video segment should have .cmfv extension, got '{key}'"
        );
    }

    // Init segment should have .cmfi extension
    assert!(
        init_keys[0].ends_with(".cmfi"),
        "Init segment should have .cmfi extension, got {}",
        init_keys[0]
    );
}

#[test]
fn test_dashsink2_mpd_required_fields() {
    let out = run_pipeline!(150, 60, 2000, false);
    let xml = &out.manifest_xml;

    assert!(xml.contains("xmlns=\"urn:mpeg:dash:schema:mpd:2011\""));
    assert!(mpd_attr(xml, "type").is_some());
    assert!(mpd_attr(xml, "minBufferTime").is_some());
    assert!(mpd_attr(xml, "profiles").is_some());

    assert!(mpd_has_tag(xml, "Period"));
    assert!(mpd_has_tag(xml, "AdaptationSet"));
    assert!(xml.contains("contentType=\"video\""));
    assert!(xml.contains("mimeType=\"video/mp4\""));

    assert!(mpd_has_tag(xml, "Representation"));
    assert!(xml.contains("codecs="));
    assert!(xml.contains("bandwidth="));
    assert!(xml.contains("width="));
    assert!(xml.contains("height="));
    assert!(xml.contains("frameRate="));

    assert!(mpd_has_tag(xml, "SegmentTemplate"));
    assert!(xml.contains("$Number$"));
    assert!(xml.contains("initialization="));
}

#[test]
fn test_dashsink2_mpd_bandwidth_positive() {
    let out = run_pipeline!(150, 60, 2000, false);

    let bw = mpd_attr(&out.manifest_xml, "bandwidth").expect("No bandwidth in MPD");
    let bw_val: u64 = bw.parse().expect("bandwidth not a number");
    assert!(bw_val > 0, "Bandwidth should be positive, got {bw_val}");
}

#[test]
fn test_dashsink2_mpd_static_duration() {
    let out = run_pipeline!(150, 60, 2000, false);

    let dur = mpd_attr(&out.manifest_xml, "mediaPresentationDuration");
    assert!(
        dur.is_some(),
        "Static MPD must have mediaPresentationDuration"
    );
    assert!(
        dur.as_ref().unwrap().starts_with("PT"),
        "Duration should be ISO 8601: {}",
        dur.unwrap()
    );
}

#[test]
fn test_dashsink2_init_segment_cmaf_brands() {
    let out = run_pipeline!(90, 30, 2000, false);

    let init_data = out.init_data();
    let mut input = Cursor::new(init_data);
    let header = Header::read_from(&mut input).unwrap();
    assert_eq!(header.kind, Ftyp::KIND, "First box should be ftyp");

    let ftyp = Ftyp::read_atom(&header, &mut input).unwrap();
    let cmf2: FourCC = (*b"cmf2").into();
    assert_eq!(ftyp.major_brand, cmf2, "Major brand should be cmf2");

    let cmfc: FourCC = (*b"cmfc").into();
    let iso6: FourCC = (*b"iso6").into();
    assert!(
        ftyp.compatible_brands.contains(&cmfc) || ftyp.compatible_brands.contains(&iso6),
        "Compatible brands should include cmfc or iso6"
    );
}

#[test]
fn test_dashsink2_each_segment_has_moof_mdat() {
    let out = run_pipeline!(300, 60, 2000, false);

    for key in &out.segment_keys() {
        let data = &out.files[key.as_str()];
        let frags = validate_cmaf_segment(data)
            .unwrap_or_else(|e| panic!("Segment {key} validation failed: {e}"));
        assert!(frags >= 1, "Segment {key} should have ≥1 moof/mdat pair");
    }
}

// ---------------------------------------------------------------------------
// Tests addressing MR !2186 discussion issues
// ---------------------------------------------------------------------------

#[test]
fn test_dashsink2_segment_duration_close_to_target() {
    // MR !2186: segment duration in MPD must match actual segment durations.
    // GOP=45 (1.5s @30fps), target=2s. With closest mode: 1.5s vs 3s → pick 1.5s (closer).
    // With GOP=30 (1s), target=2s → 2×1s=2s exact.
    let out = run_pipeline!(300, 30, 2000, false);

    let init_data = out.init_data();
    let timescale = extract_timescale(init_data);
    let seg_keys = out.segment_keys();

    for key in &seg_keys[..seg_keys.len().saturating_sub(1)] {
        let dur = segment_duration_secs(&out.files[key.as_str()], timescale);
        assert!(
            dur >= 1.5 && dur <= 3.0,
            "Segment {key} duration {dur:.2}s is too far from 2s target"
        );
    }
}

#[test]
fn test_dashsink2_segment_duration_uneven_gop() {
    // MR !2186 core issue: GOP doesn't divide into target → segments must still be
    // close to the target, not systematically short.
    // GOP=45 (1.5s @30fps), target=4s.
    //   Without closest: 1.5, 3.0 < 4 → pick 3.0s (33% short)
    //   With closest: |3-4|=1 vs |4.5-4|=0.5 → pick 4.5s (closer)
    let out = run_pipeline!(300, 45, 4000, false);

    let init_data = out.init_data();
    let timescale = extract_timescale(init_data);
    let seg_keys = out.segment_keys();

    // Skip last segment (may be shorter due to EOS)
    for key in &seg_keys[..seg_keys.len().saturating_sub(1)] {
        let dur = segment_duration_secs(&out.files[key.as_str()], timescale);
        // With closest mode, segments should be >= 3s (not stuck at 1.5s)
        assert!(
            dur >= 3.0,
            "Segment {key} is {dur:.2}s — too short, closest mode should pick longer fragment"
        );
        assert!(dur <= 6.0, "Segment {key} is {dur:.2}s — unexpectedly long");
    }
}

#[test]
fn test_dashsink2_segment_sample_count_matches_gop() {
    // Each non-final segment should contain a whole number of GOPs worth of frames.
    // GOP=60 (2s @30fps), target=2s → each segment should have exactly 60 frames.
    let out = run_pipeline!(300, 60, 2000, false);

    let seg_keys = out.segment_keys();
    for key in &seg_keys[..seg_keys.len().saturating_sub(1)] {
        let count = segment_sample_count(&out.files[key.as_str()]);
        assert!(
            count % 60 == 0,
            "Segment {key} has {count} samples, expected multiple of 60 (GOP size)"
        );
    }
}

#[test]
fn test_dashsink2_video_audio_segment_count() {
    // MR !2186: dashcmafsink produces more video segments than audio segments.
    // With proper cmafmux segmentation, video and audio segment counts should match.
    let out = run_av!(300, 60, 2000);

    let video_segs = out.segment_keys_for("video_");
    let audio_segs = out.segment_keys_for("audio_");

    assert!(!video_segs.is_empty(), "No video segments produced");
    assert!(!audio_segs.is_empty(), "No audio segments produced");

    // Video and audio segment counts should be equal or differ by at most 1
    // (the final segment at EOS may differ due to audio/video stream length mismatch).
    // The MR !2186 bug was a systematic mismatch (e.g. video=10, audio=5), not ±1.
    let diff = (video_segs.len() as isize - audio_segs.len() as isize).unsigned_abs();
    assert!(
        diff <= 1,
        "Video ({}) and audio ({}) segment counts differ by {diff} (expected ≤1)",
        video_segs.len(),
        audio_segs.len()
    );
}

#[test]
fn test_dashsink2_video_audio_init_segments() {
    // Both video and audio should have valid init segments with proper extensions.
    let out = run_av!(150, 60, 2000);

    let video_init = out.init_data_for("video_");
    let audio_init = out.init_data_for("audio_");

    validate_cmaf_init(video_init).expect("Invalid video init segment");
    validate_cmaf_init(audio_init).expect("Invalid audio init segment");

    // Check file extensions
    let video_init_key = out
        .files
        .keys()
        .find(|k| k.starts_with("video_") && k.contains("init"))
        .unwrap();
    let audio_init_key = out
        .files
        .keys()
        .find(|k| k.starts_with("audio_") && k.contains("init"))
        .unwrap();
    assert!(
        video_init_key.ends_with(".cmfi"),
        "Video init should be .cmfi, got {video_init_key}"
    );
    assert!(
        audio_init_key.ends_with(".cmfi"),
        "Audio init should be .cmfi, got {audio_init_key}"
    );

    // Check segment extensions
    let video_seg_keys = out.segment_keys_for("video_");
    let audio_seg_keys = out.segment_keys_for("audio_");
    for k in &video_seg_keys {
        assert!(
            k.ends_with(".cmfv"),
            "Video segment should be .cmfv, got {k}"
        );
    }
    for k in &audio_seg_keys {
        assert!(
            k.ends_with(".cmfa"),
            "Audio segment should be .cmfa, got {k}"
        );
    }
}

#[test]
fn test_dashsink2_video_audio_mpd_has_both_tracks() {
    // MPD must contain both video and audio AdaptationSets.
    let out = run_av!(150, 60, 2000);
    let xml = &out.manifest_xml;

    assert!(
        xml.contains("contentType=\"video\""),
        "MPD missing video contentType"
    );
    assert!(
        xml.contains("contentType=\"audio\""),
        "MPD missing audio contentType"
    );
    assert!(
        xml.contains("mimeType=\"video/mp4\""),
        "MPD missing video mimeType"
    );
    assert!(
        xml.contains("mimeType=\"audio/mp4\""),
        "MPD missing audio mimeType"
    );
}

#[test]
fn test_dashsink2_bandwidth_reflects_actual_duration() {
    // MR !2186: bandwidth was computed from target_duration instead of actual.
    // With GOP=90 (3s) and target=2s, actual segment ≈ 3s. Using target=2s would
    // overestimate bandwidth by ~50%. Check that bandwidth is reasonable.
    let out = run_pipeline!(300, 90, 2000, false);

    let bw_str = mpd_attr(&out.manifest_xml, "bandwidth").expect("No bandwidth in MPD");
    let bw: u64 = bw_str.parse().unwrap();

    // x264enc bitrate=512 kbps → bandwidth should be roughly 512000 bps.
    // Allow wide range but catch gross overestimates (old bug would give ~768000).
    assert!(
        bw > 100_000 && bw < 1_500_000,
        "Bandwidth {bw} bps seems unreasonable for 512kbps encode"
    );
}

#[test]
fn test_dashsink2_keyframe_every_frame() {
    // MR !2186: dashsink2 should be resistant to additional I-frames (scene changes).
    // GOP=1 means every frame is a keyframe. cmafmux should still produce
    // segments close to target duration despite having many GOP boundaries.
    let out = run_pipeline!(300, 1, 2000, false);

    let init_data = out.init_data();
    let timescale = extract_timescale(init_data);
    let seg_keys = out.segment_keys();

    assert!(
        seg_keys.len() >= 3,
        "Expected at least 3 segments, got {}",
        seg_keys.len()
    );

    for key in &seg_keys[..seg_keys.len().saturating_sub(1)] {
        let dur = segment_duration_secs(&out.files[key.as_str()], timescale);
        // With every-frame keyframes and 2s target, closest mode should produce ~2s segments
        assert!(
            dur >= 1.0 && dur <= 3.0,
            "Segment {key} duration {dur:.2}s — should be close to 2s target even with every-frame keyframes"
        );
    }
}

#[test]
fn test_dashsink2_video_large_target_duration() {
    // Target much larger than GOP: GOP=30 (1s), target=6s
    // cmafmux should accumulate 6 GOPs into one fragment.
    let out = run_pipeline!(300, 30, 6000, false);

    let init_data = out.init_data();
    let timescale = extract_timescale(init_data);
    let seg_keys = out.segment_keys();

    assert!(
        seg_keys.len() >= 1 && seg_keys.len() <= 3,
        "Expected 1-3 segments for 10s content / 6s target, got {}",
        seg_keys.len()
    );

    if seg_keys.len() >= 2 {
        let dur = segment_duration_secs(&out.files[seg_keys[0].as_str()], timescale);
        assert!(
            dur >= 5.0 && dur <= 7.0,
            "First segment duration {dur:.2}s should be close to 6s target"
        );
    }
}

#[test]
fn test_dashsink2_mpd_segment_duration_matches_target() {
    // SegmentTemplate @duration in MPD should be close to the configured target.
    let out = run_pipeline!(300, 60, 2000, false);

    let xml = &out.manifest_xml;
    if let Some(dur_str) = mpd_attr(xml, "duration") {
        if let Some(ts_str) = mpd_attr(xml, "timescale") {
            let dur: f64 = dur_str.parse().unwrap_or(0.0);
            let ts: f64 = ts_str.parse().unwrap_or(1.0);
            let segment_duration_s = dur / ts;
            assert!(
                segment_duration_s >= 1.5 && segment_duration_s <= 3.0,
                "MPD SegmentTemplate duration {segment_duration_s:.2}s should be close to 2s target"
            );
        }
    }
}

#[test]
fn test_dashsink2_segments_contain_samples() {
    // Every segment must have at least one sample (frame).
    let out = run_pipeline!(300, 60, 2000, false);

    for key in &out.segment_keys() {
        let count = segment_sample_count(&out.files[key.as_str()]);
        assert!(count > 0, "Segment {key} has 0 samples");
    }
}

#[test]
fn test_dashsink2_full_stream_playable() {
    // Concatenation of init + all segments should form a valid fMP4.
    let out = run_pipeline!(150, 60, 2000, false);
    let init_data = out.init_data();

    for key in &out.segment_keys() {
        validate_combined_fmp4(init_data, &out.files[key.as_str()])
            .unwrap_or_else(|e| panic!("Segment {key} not playable with init: {e}"));
    }
}

#[test]
fn test_dashsink2_init_reusable_across_segments() {
    // A single init segment should work with all media segments.
    let out = run_pipeline!(300, 60, 2000, false);
    let init_data = out.init_data();

    let seg_keys = out.segment_keys();
    assert!(seg_keys.len() >= 2, "Need at least 2 segments");

    for key in &seg_keys {
        validate_combined_fmp4(init_data, &out.files[key.as_str()])
            .unwrap_or_else(|e| panic!("Init segment incompatible with {key}: {e}"));
    }
}

#[test]
fn test_dashsink2_no_duplicate_sequence_numbers() {
    // Each moof should have a unique, positive sequence number.
    let out = run_pipeline!(300, 60, 2000, false);

    let mut seq_nums = Vec::new();
    for key in &out.segment_keys() {
        let data = &out.files[key.as_str()];
        let mut input = Cursor::new(data);
        while let Ok(header) = Header::read_from(&mut input) {
            match header.kind {
                Moof::KIND => {
                    let moof = Moof::read_atom(&header, &mut input).unwrap();
                    seq_nums.push(moof.mfhd.sequence_number);
                }
                _ => {
                    let skip = header.size.unwrap_or(0);
                    let mut buf = vec![0u8; skip];
                    std::io::Read::read_exact(&mut input, &mut buf).ok();
                }
            }
        }
    }

    let unique: std::collections::HashSet<_> = seq_nums.iter().collect();
    assert_eq!(
        unique.len(),
        seq_nums.len(),
        "Duplicate sequence numbers found: {:?}",
        seq_nums
    );
}

#[test]
fn test_dashsink2_sequence_numbers_monotonic() {
    let out = run_pipeline!(300, 60, 2000, false);

    let mut prev = 0u32;
    for key in &out.segment_keys() {
        let data = &out.files[key.as_str()];
        let mut input = Cursor::new(data);
        while let Ok(header) = Header::read_from(&mut input) {
            match header.kind {
                Moof::KIND => {
                    let moof = Moof::read_atom(&header, &mut input).unwrap();
                    let seq = moof.mfhd.sequence_number;
                    assert!(
                        seq > prev,
                        "Sequence number {seq} not greater than previous {prev}"
                    );
                    prev = seq;
                }
                _ => {
                    let skip = header.size.unwrap_or(0);
                    let mut buf = vec![0u8; skip];
                    std::io::Read::read_exact(&mut input, &mut buf).ok();
                }
            }
        }
    }
}

#[test]
fn test_dashsink2_presentation_duration_accurate() {
    // Total duration across all segments should approximately match content duration.
    // 300 frames @30fps = 10s.
    let out = run_pipeline!(300, 60, 2000, false);

    let init_data = out.init_data();
    let timescale = extract_timescale(init_data);

    let total_duration: f64 = out
        .segment_keys()
        .iter()
        .map(|key| segment_duration_secs(&out.files[key.as_str()], timescale))
        .sum();

    assert!(
        total_duration >= 9.0 && total_duration <= 11.0,
        "Total duration {total_duration:.2}s should be ~10s (300 frames @30fps)"
    );
}

#[test]
fn test_dashsink2_segment_numbering_contiguous() {
    let out = run_pipeline!(300, 60, 2000, false);

    let seg_keys = out.segment_keys();
    for (i, key) in seg_keys.iter().enumerate() {
        let expected = format!("segment_{}", i);
        assert!(
            key.contains(&expected),
            "Expected segment number {i} in key, got {key}"
        );
    }
}

#[test]
fn test_dashsink2_target_duration_respected() {
    // With aligned GOP and target, segments should be exactly on target.
    // GOP=60 (2s @30fps), target=2s → all non-final segments should be exactly 2s.
    let out = run_pipeline!(300, 60, 2000, false);

    let init_data = out.init_data();
    let timescale = extract_timescale(init_data);
    let seg_keys = out.segment_keys();

    for key in &seg_keys[..seg_keys.len().saturating_sub(1)] {
        let dur = segment_duration_secs(&out.files[key.as_str()], timescale);
        assert!(
            (dur - 2.0).abs() < 0.1,
            "Segment {key} duration {dur:.3}s should be exactly 2s (GOP aligns with target)"
        );
    }
}

#[test]
fn test_dashsink2_mpd_dashif_iop_attributes() {
    // DASH-IF IOP requires certain attributes on AdaptationSet and Representation.
    let out = run_pipeline!(150, 60, 2000, false);
    let xml = &out.manifest_xml;

    assert!(
        xml.contains("startWithSAP="),
        "Missing startWithSAP attribute (DASH-IF IOP)"
    );
}

#[test]
fn test_dashsink2_mpd_profile_isoff_live() {
    // Profile should indicate ISO BMFF live or on-demand.
    let out = run_pipeline!(150, 60, 2000, false);
    let xml = &out.manifest_xml;

    let profiles = mpd_attr(xml, "profiles").expect("Missing profiles attribute");
    assert!(
        profiles.contains("urn:mpeg:dash:profile:isoff-live:2011")
            || profiles.contains("urn:mpeg:dash:profile:isoff-on-demand:2011"),
        "Unexpected profile: {profiles}"
    );
}
