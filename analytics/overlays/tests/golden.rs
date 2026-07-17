// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Golden-image regression tests.
//
// Each test renders a fixed synthetic scene through one of the overlay elements
// and compares the output, pixel for pixel, against a committed reference PNG
// under `tests/golden/`. This is a *self-referential* regression guard: it
// catches unintended changes in our own rendering (glyph shaping, antialiasing,
// colour, layout) that the command-level behaviour tests cannot see. It is not
// a comparison against the C elements.
//
// Determinism rests on three fixed inputs: synthetic metadata (no models), the
// embedded label font (see `fonts/README.md`), and the pinned `skia-safe`
// version (which vendors its own freetype/harfbuzz). Bumping `skia-safe` may
// legitimately change rasterisation; regenerate the goldens when that happens:
//
//     BLESS_GOLDEN=1 cargo test -p gst-plugin-overlays --test golden
//
// A missing golden is created (and the test fails once, to flag it for review).
// On a mismatch the actual and a difference image are written to the test
// temp dir (printed in the failure message) for inspection.
//
// Linux only: text rendering goes through skia's platform-native font backend
// (freetype on Linux, DirectWrite on Windows, CoreText on macOS), so glyphs are
// not pixel-identical across operating systems and the committed PNGs only match
// on the platform they were blessed on. As a self-referential regression guard,
// running on the one canonical platform (Linux, where CI blesses) is sufficient;
// the command-level behaviour tests still cover rendering on every OS.
#![cfg(target_os = "linux")]

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use gst_analytics::{
    AnalyticsKeypointDimensions, AnalyticsKeypointPosition, AnalyticsKeypointVisibility,
    AnalyticsRelationMetaGroupExt, AnalyticsRelationMetaKeypointExt, AnalyticsRelationMetaODExt,
    RelTypes,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const STRIDE: usize = WIDTH as usize * 4;
const BUFFER_SIZE: usize = STRIDE * HEIGHT as usize;

/// Maximum allowed per-channel difference for any pixel. Output is bit-exact
/// run to run on a given build, so this only absorbs trivial cross-machine
/// rounding; a real regression moves edges by far more than this.
const PIXEL_TOLERANCE: u8 = 8;

fn init() {
    use std::sync::Once;

    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlays::plugin_register_static().expect("overlays test");
    });
}

fn caps_str() -> String {
    format!("video/x-raw,format=BGRA,width={WIDTH},height={HEIGHT},framerate=1/1")
}

/// A buffer filled with a fixed two-axis colour gradient, so labels and shapes
/// are blended over varied (deterministic) background colours.
fn background_buffer() -> gst::Buffer {
    let mut data = vec![0_u8; BUFFER_SIZE];
    for y in 0..HEIGHT as usize {
        for x in 0..WIDTH as usize {
            let i = y * STRIDE + x * 4;
            // BGRA byte order.
            data[i] = (x * 255 / (WIDTH as usize - 1)) as u8; // B
            data[i + 1] = (y * 255 / (HEIGHT as usize - 1)) as u8; // G
            data[i + 2] = 128; // R
            data[i + 3] = 255; // A
        }
    }
    let mut buffer = gst::Buffer::from_mut_slice(data);
    buffer.get_mut().unwrap().set_pts(gst::ClockTime::ZERO);
    buffer
}

/// Render one synthetic scene: build a harness for `factory`, apply `props`,
/// attach metadata via `build_meta`, push a gradient frame, and return the
/// rendered output as tightly packed RGBA bytes.
fn render_scene(
    factory: &str,
    props: &[(&str, glib::Value)],
    build_meta: impl FnOnce(&mut gst::BufferRef),
) -> Vec<u8> {
    init();

    let mut harness = gst_check::Harness::new(factory);
    harness.set_src_caps_str(&caps_str());
    harness.set_sink_caps_str(&caps_str());

    {
        let element = harness.element().unwrap();
        element.set_property("render-enabled", true);
        for (name, value) in props {
            element.set_property_from_value(name, value);
        }
    }

    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(harness.push_event(gst::event::Segment::builder(&segment).build()));

    let mut buffer = background_buffer();
    build_meta(buffer.get_mut().unwrap());

    assert_eq!(harness.push(buffer), Ok(gst::FlowSuccess::Ok));
    let output = harness.pull().unwrap();

    let map = output.map_readable().unwrap();
    let src = map.as_slice();
    let mut rgba = vec![0_u8; WIDTH as usize * HEIGHT as usize * 4];
    for y in 0..HEIGHT as usize {
        for x in 0..WIDTH as usize {
            let s = y * STRIDE + x * 4;
            let d = (y * WIDTH as usize + x) * 4;
            rgba[d] = src[s + 2]; // R
            rgba[d + 1] = src[s + 1]; // G
            rgba[d + 2] = src[s]; // B
            rgba[d + 3] = src[s + 3]; // A
        }
    }
    rgba
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.png"))
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) {
    use std::io::BufWriter;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(rgba).unwrap();
}

fn read_png(path: &std::path::Path) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0_u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!(
        info.color_type,
        png::ColorType::Rgba,
        "golden {path:?} is not RGBA"
    );
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// Compare `actual` RGBA against the committed golden for `name`. Honours
/// `BLESS_GOLDEN` to (re)write goldens; creates a missing golden and fails once.
fn assert_matches_golden(name: &str, actual: &[u8]) {
    let path = golden_path(name);
    let blessing = std::env::var_os("BLESS_GOLDEN").is_some();

    if blessing {
        write_png(&path, WIDTH, HEIGHT, actual);
        eprintln!("blessed golden {name} -> {path:?}");
        return;
    }

    if !path.exists() {
        write_png(&path, WIDTH, HEIGHT, actual);
        panic!("golden {name} did not exist; created {path:?} — review it and re-run");
    }

    let (gw, gh, golden) = read_png(&path);
    assert_eq!(
        (gw, gh),
        (WIDTH, HEIGHT),
        "golden {name} has unexpected dimensions"
    );

    let mut max_diff = 0_u8;
    let mut differing_pixels = 0_usize;
    for (a, g) in actual.chunks_exact(4).zip(golden.chunks_exact(4)) {
        let pixel_diff = a
            .iter()
            .zip(g)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0);
        max_diff = max_diff.max(pixel_diff);
        if pixel_diff > PIXEL_TOLERANCE {
            differing_pixels += 1;
        }
    }

    if differing_pixels > 0 {
        // `GOLDEN_DIFF_DIR` lets CI point the dumps at a path it collects as
        // artifacts; otherwise fall back to the per-test temp dir.
        let dir = std::env::var_os("GOLDEN_DIFF_DIR")
            .or_else(|| std::env::var_os("CARGO_TARGET_TMPDIR"))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let actual_path = dir.join(format!("{name}.actual.png"));
        let diff_path = dir.join(format!("{name}.diff.png"));
        write_png(&actual_path, WIDTH, HEIGHT, actual);

        // Difference image: per-channel absolute difference, opaque.
        let mut diff = vec![0_u8; actual.len()];
        for ((a, g), d) in actual
            .chunks_exact(4)
            .zip(golden.chunks_exact(4))
            .zip(diff.chunks_exact_mut(4))
        {
            for c in 0..3 {
                d[c] = a[c].abs_diff(g[c]);
            }
            d[3] = 255;
        }
        write_png(&diff_path, WIDTH, HEIGHT, &diff);

        panic!(
            "golden {name} mismatch: {differing_pixels} pixel(s) exceed tolerance \
             {PIXEL_TOLERANCE} (max channel diff {max_diff}).\n  actual: {actual_path:?}\n  \
             diff:   {diff_path:?}\nIf this change is intended, re-bless with \
             BLESS_GOLDEN=1."
        );
    }
}

// --- Object detection ----------------------------------------------------

#[test]
fn golden_od_labeled_boxes() {
    let rgba = render_scene(
        "odoverlay",
        &[("draw-tracking-labels", false.into())],
        |buffer| {
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer);
            relation
                .add_od_mtd(glib::Quark::from_str("person"), 40, 30, 70, 90, 0.94)
                .unwrap();
            relation
                .add_od_mtd(glib::Quark::from_str("car"), 180, 60, 110, 70, 0.88)
                .unwrap();
            relation
                .add_od_mtd(glib::Quark::from_str("dog"), 60, 150, 80, 60, 0.76)
                .unwrap();
        },
    );
    assert_matches_golden("od_labeled_boxes", &rgba);
}

#[test]
fn golden_od_overlapping_cluster() {
    // Overlapping boxes exercise label avoidance and leader lines.
    let rgba = render_scene(
        "odoverlay",
        &[("draw-tracking-labels", false.into())],
        |buffer| {
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer);
            relation
                .add_od_mtd(glib::Quark::from_str("car"), 60, 60, 100, 80, 0.91)
                .unwrap();
            relation
                .add_od_mtd(glib::Quark::from_str("car"), 120, 95, 100, 80, 0.85)
                .unwrap();
            relation
                .add_od_mtd(glib::Quark::from_str("person"), 150, 70, 60, 120, 0.79)
                .unwrap();
        },
    );
    assert_matches_golden("od_overlapping_cluster", &rgba);
}

// --- Keypoints -----------------------------------------------------------

#[test]
fn golden_keypoints_generic() {
    let rgba = render_scene("keypointsoverlay", &[], |buffer| {
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer);
        for (x, y, c) in [
            (60, 60, 0.90_f32),
            (160, 80, 0.80),
            (240, 150, 0.70),
            (100, 180, 0.95),
        ] {
            relation
                .add_keypoint_mtd(
                    AnalyticsKeypointDimensions::_2d,
                    x,
                    y,
                    0,
                    AnalyticsKeypointVisibility::VISIBLE,
                    c,
                )
                .unwrap();
        }
    });
    assert_matches_golden("keypoints_generic", &rgba);
}

#[test]
fn golden_keypoints_hand_kp_21() {
    // 21-point hand: the specialised renderer draws its own bone topology.
    const HAND: [(i32, i32); 21] = [
        (160, 215), // 0 wrist
        (120, 200),
        (100, 180),
        (88, 160),
        (80, 140), // thumb
        (135, 150),
        (130, 120),
        (127, 100),
        (125, 82), // index
        (160, 145),
        (160, 112),
        (160, 90),
        (160, 70), // middle
        (185, 150),
        (190, 120),
        (193, 100),
        (195, 82), // ring
        (208, 160),
        (216, 135),
        (221, 118),
        (225, 102), // pinky
    ];
    let rgba = render_scene(
        "keypointsoverlay",
        &[
            ("draw-skeleton", true.into()),
            // Selecting a semantic tag routes the group through the specialised
            // renderer (skeleton from the fixed bone topology, one group label).
            ("semantic-tag", "hand-kp-21".into()),
        ],
        |buffer| {
            let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer);
            let positions = HAND
                .iter()
                .map(|(x, y)| AnalyticsKeypointPosition {
                    x: *x,
                    y: *y,
                    z: 0,
                    dimension: AnalyticsKeypointDimensions::_2d,
                })
                .collect::<Vec<_>>();
            relation
                .add_keypoints_group_from_positions("hand-kp-21", &positions, None, None, &[])
                .unwrap();
        },
    );
    assert_matches_golden("keypoints_hand_kp_21", &rgba);
}

// --- Segmentation --------------------------------------------------------

fn make_mask_buffer(width: u32, height: u32, values: Vec<u8>) -> gst::Buffer {
    let mut mask = gst::Buffer::from_mut_slice(values);
    gst_video::VideoMeta::add(
        mask.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Gray8,
        width,
        height,
    )
    .unwrap();
    mask
}

fn add_segmentation_mtd(
    relation: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    mask: gst::Buffer,
    loc_x: i32,
    loc_y: i32,
    loc_w: u32,
    loc_h: u32,
) -> u32 {
    let mut region_ids = vec![0_u32, 1_u32, 2_u32, 3_u32];
    let mut seg_mtd =
        std::mem::MaybeUninit::<gst_analytics::ffi::GstAnalyticsSegmentationMtd>::uninit();

    let success: bool = unsafe {
        from_glib(
            gst_analytics::ffi::gst_analytics_relation_meta_add_segmentation_mtd(
                relation.as_mut_ptr(),
                mask.into_glib_ptr(),
                gst_analytics::ffi::GST_SEGMENTATION_TYPE_SEMANTIC,
                region_ids.len(),
                region_ids.as_mut_ptr(),
                loc_x,
                loc_y,
                loc_w,
                loc_h,
                seg_mtd.as_mut_ptr(),
            ),
        )
    };
    assert!(success, "failed to add segmentation metadata");

    let seg_mtd = unsafe { seg_mtd.assume_init() };
    seg_mtd.id
}

#[test]
fn golden_segmentation_masks() {
    use gst_analytics::AnalyticsRelationMetaClassificationExt;

    let rgba = render_scene("segoverlay", &[], |buffer| {
        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer);

        let segments: [(gst::Buffer, i32, i32, u32, u32); 2] = [
            (make_mask_buffer(8, 8, vec![1_u8; 64]), 40, 40, 120, 120),
            (make_mask_buffer(8, 8, vec![2_u8; 64]), 150, 90, 140, 110),
        ];
        for (mask, x, y, w, h) in segments {
            let seg_id = add_segmentation_mtd(&mut relation, mask, x, y, w, h);
            let classes = [
                glib::Quark::from_str("background"),
                glib::Quark::from_str("person"),
                glib::Quark::from_str("car"),
                glib::Quark::from_str("dog"),
            ];
            let levels = [1.0_f32; 4];
            let cls_id = relation.add_cls_mtd(&levels, &classes).unwrap().id();
            relation
                .set_relation(RelTypes::N_TO_N, seg_id, cls_id)
                .unwrap();
        }
    });
    assert_matches_golden("segmentation_masks", &rgba);
}
