// Copyright (C) 2026 Jeremy Whiting <jeremy.whiting@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use glib::translate::{IntoGlibPtr, from_glib};
use gst::prelude::*;
use gst_analytics::{AnalyticsRelationMetaClassificationExt, RelTypes};

const WIDTH: usize = 64;
const HEIGHT: usize = 64;
const STRIDE: usize = WIDTH * 4;
const BUFFER_SIZE: usize = STRIDE * HEIGHT;

fn init() {
    use std::sync::Once;

    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstoverlays::plugin_register_static().expect("overlays test");
    });
}

fn make_harness(selected_types: Option<&str>) -> gst_check::Harness {
    let mut harness = gst_check::Harness::new("segoverlay");
    harness.set_src_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");
    harness.set_sink_caps_str("video/x-raw,format=BGRA,width=64,height=64,framerate=1/1");

    let element = harness.element().unwrap();
    element.set_property("render-enabled", true);
    if let Some(selected_types) = selected_types {
        element.set_property("selected-types", selected_types);
    }

    push_time_segment_to_harness(&mut harness);

    harness
}

fn push_time_segment_to_harness(harness: &mut gst_check::Harness) {
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(harness.push_event(gst::event::Segment::builder(&segment).build()));
}

fn make_pipeline(
    selected_types: Option<&str>,
) -> (gst::Pipeline, gst_app::AppSrc, gst_app::AppSink) {
    make_pipeline_with_options(selected_types, false)
}

fn make_pipeline_with_options(
    selected_types: Option<&str>,
    with_overlay_meta_feature: bool,
) -> (gst::Pipeline, gst_app::AppSrc, gst_app::AppSink) {
    let pipeline = gst::Pipeline::new();
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "BGRA")
        .field("width", WIDTH as i32)
        .field("height", HEIGHT as i32)
        .field("framerate", gst::Fraction::new(1, 1))
        .build();

    let mut downstream_caps = caps.clone();
    if with_overlay_meta_feature && let Some(features) = downstream_caps.make_mut().features_mut(0)
    {
        features.add(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
    }

    let appsrc = gst_app::AppSrc::builder()
        .name("src")
        .caps(&caps)
        .format(gst::Format::Time)
        .build();

    let mut overlay_builder = gst::ElementFactory::make("segoverlay");
    overlay_builder = overlay_builder.name("overlay");
    overlay_builder = overlay_builder.property("render-enabled", true);
    if let Some(selected_types) = selected_types {
        overlay_builder = overlay_builder.property("selected-types", selected_types);
    }
    let overlay = overlay_builder.build().unwrap();

    let appsink = gst_app::AppSink::builder().name("sink").sync(false).build();

    if with_overlay_meta_feature {
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &downstream_caps)
            .build()
            .unwrap();

        pipeline
            .add_many([
                appsrc.upcast_ref(),
                &overlay,
                &capsfilter,
                appsink.upcast_ref(),
            ])
            .unwrap();
        gst::Element::link_many([
            appsrc.upcast_ref(),
            &overlay,
            &capsfilter,
            appsink.upcast_ref(),
        ])
        .unwrap();
    } else {
        appsink.set_caps(Some(&caps));

        pipeline
            .add_many([appsrc.upcast_ref(), &overlay, appsink.upcast_ref()])
            .unwrap();
        gst::Element::link_many([appsrc.upcast_ref(), &overlay, appsink.upcast_ref()]).unwrap();
    }

    (pipeline, appsrc, appsink)
}

// Upper bound on live-pipeline waits. Generous enough to never false-fail under
// CI load, but bounded so a stuck pipeline fails fast with a clear message
// instead of hanging until the test runner's (much longer) timeout.
const PIPELINE_WAIT: gst::ClockTime = gst::ClockTime::from_seconds(30);

fn wait_for_pipeline_eos(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();

    match bus.timed_pop_filtered(
        PIPELINE_WAIT,
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(message) => {
            if let gst::MessageView::Error(err) = message.view() {
                panic!(
                    "pipeline error from {:?}: {} ({:?})",
                    err.src().map(|src| src.path_string()),
                    err.error(),
                    err.debug()
                );
            }
        }
        None => panic!("timed out waiting for EOS on the pipeline bus"),
    }
}

fn pull_buffer_from_appsink(appsink: &gst_app::AppSink) -> gst::Buffer {
    appsink
        .try_pull_sample(PIPELINE_WAIT)
        .expect("timed out waiting for a sample from appsink")
        .buffer()
        .unwrap()
        .copy()
}

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

fn make_segmented_buffer(
    pts: gst::ClockTime,
    segments: Vec<(gst::Buffer, i32, i32, u32, u32)>,
    attach_classification: bool,
) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(pts);

        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);

        for (mask, x, y, w, h) in segments {
            let seg_id = add_segmentation_mtd(&mut relation, mask, x, y, w, h);

            if attach_classification {
                let classes = [
                    glib::Quark::from_str("background"),
                    glib::Quark::from_str("person"),
                    glib::Quark::from_str("car"),
                    glib::Quark::from_str("dog"),
                ];
                let levels = [1.0_f32, 1.0_f32, 1.0_f32, 1.0_f32];
                let cls_id = {
                    let cls = relation.add_cls_mtd(&levels, &classes).unwrap();
                    cls.id()
                };
                relation
                    .set_relation(RelTypes::N_TO_N, seg_id, cls_id)
                    .unwrap();
            }
        }
    }

    buffer
}

fn make_plain_buffer(pts: gst::ClockTime) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    buffer.get_mut().unwrap().set_pts(pts);
    buffer
}

fn attach_upstream_overlay_rect(
    buffer: &mut gst::BufferRef,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    bgra: [u8; 4],
) {
    let mut pixels = vec![0_u8; (w as usize) * (h as usize) * 4];
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&bgra);
    }

    let mut overlay_buf = gst::Buffer::from_mut_slice(pixels);
    gst_video::VideoMeta::add(
        overlay_buf.get_mut().unwrap(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Bgra,
        w,
        h,
    )
    .unwrap();

    let rect = gst_video::VideoOverlayRectangle::new_raw(
        &overlay_buf,
        x,
        y,
        w,
        h,
        gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
    );
    let comp = gst_video::VideoOverlayComposition::new(Some(&rect)).unwrap();
    gst_video::VideoOverlayCompositionMeta::add(buffer, &comp);
}

fn count_nonzero_alpha(buffer: &gst::Buffer) -> usize {
    let map = buffer.map_readable().unwrap();
    map.as_slice()
        .chunks_exact(4)
        .filter(|px| px[3] != 0)
        .count()
}

fn pixel_bgra(buffer: &gst::Buffer, x: usize, y: usize) -> [u8; 4] {
    let map = buffer.map_readable().unwrap();
    let offset = y * STRIDE + x * 4;
    let data = &map.as_slice()[offset..offset + 4];
    [data[0], data[1], data[2], data[3]]
}

fn buffer_has_drawn_pixels(buffer: &gst::Buffer) -> bool {
    count_nonzero_alpha(buffer) > 0
}

#[test]
fn pipeline_small_mask_vector_renders_overlay() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask = make_mask_buffer(4, 4, vec![0, 1, 1, 0, 1, 1, 1, 1, 0, 1, 1, 0, 0, 0, 1, 0]);
    let input = make_segmented_buffer(gst::ClockTime::ZERO, vec![(mask, 8, 8, 12, 12)], true);

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    // Verify segmentation mask is rendered. At least some pixels with non-zero
    // alpha channel should be drawn to indicate the segmented region.
    assert!(count_nonzero_alpha(&out) > 0);
    // Verify pixel at (0, 0) remains untouched (alpha=0). This ensures rendering
    // is localized to the segmentation region and doesn't affect the entire frame.
    assert_eq!(pixel_bgra(&out, 0, 0)[3], 0);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn publishes_avoid_claims_for_mask_regions() {
    init();

    let mut harness = make_harness(None);

    let mask = make_mask_buffer(4, 4, vec![1; 16]);
    let input = make_segmented_buffer(gst::ClockTime::ZERO, vec![(mask, 8, 8, 12, 12)], false);
    let out = harness.push_and_pull(input).expect("pull output buffer");

    // The element publishes the regions it drew via the shared claimed-regions
    // meta so downstream overlays steer their labels away. Read it by its
    // well-known registered name (no crate-internal access needed).
    let meta = gst::meta::CustomMeta::from_buffer(&out, "GstAnalyticsClaimedRegions")
        .expect("segmentation should publish claimed regions");
    let structure = meta.structure();
    let coords = structure.get::<gst::Array>("coords").expect("coords array");
    let owners = structure.get::<gst::Array>("owners").expect("owners array");
    let coords = coords.as_slice();
    let owners = owners.as_slice();

    // The meta packs this many i32 per region: x, y, w, h, kind (0 = Occlude,
    // 1 = Avoid), priority.
    const COORDS_PER_REGION: usize = 6;
    assert_eq!(coords.len() % COORDS_PER_REGION, 0);
    let region_count = coords.len() / COORDS_PER_REGION;
    assert!(region_count >= 1, "expected at least one mask claim");

    let mut found = false;
    for i in 0..region_count {
        let coord = |j: usize| coords[i * COORDS_PER_REGION + j].get::<i32>().unwrap();
        let owner = owners[i].get::<String>().unwrap();
        if owner == "segoverlay" {
            // Masks are claimed as soft Avoid (kind == 1), at the destination
            // rect they were drawn into (clamped to the frame), at the default
            // priority.
            assert_eq!(coord(4), 1, "mask claim should be Avoid, not Occlude");
            assert_eq!((coord(0), coord(1), coord(2), coord(3)), (8, 8, 12, 12));
            assert_eq!(coord(5), 0, "default element priority");
            found = true;
        }
    }
    assert!(found, "expected a segoverlay-owned Avoid mask claim");
}

#[test]
fn pipeline_large_mask_vector_scales_over_full_frame() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask = make_mask_buffer(8, 8, vec![1_u8; 64]);
    let input = make_segmented_buffer(
        gst::ClockTime::ZERO,
        vec![(mask, 0, 0, WIDTH as u32, HEIGHT as u32)],
        true,
    );

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    // Verify large mask is scaled and rendered over the full frame bounds.
    // The element should scale the 8x8 mask to fill the 64x64 region and render
    // more than 50% of pixels with non-zero alpha (the mask is all 1s).
    assert!(count_nonzero_alpha(&out) > (WIDTH * HEIGHT) / 2);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_overlapping_masks_vector_composites_in_order() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let first_mask = make_mask_buffer(8, 8, vec![1_u8; 64]);
    let second_mask = make_mask_buffer(8, 8, vec![2_u8; 64]);
    let input = make_segmented_buffer(
        gst::ClockTime::ZERO,
        vec![(first_mask, 10, 10, 24, 24), (second_mask, 18, 18, 24, 24)],
        true,
    );

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);

    let first_only = pixel_bgra(&out, 12, 12);
    let overlap = pixel_bgra(&out, 22, 22);
    // Verify first mask region (only covered by first mask) has non-zero alpha.
    assert!(first_only[3] > 0);
    // Verify overlapping region (covered by both masks) has non-zero alpha.
    // This ensures both masks contribute to the final composite.
    assert!(overlap[3] > 0);
    // Verify pixels have different colors in non-overlapping vs overlapping regions.
    // This confirms the element composites masks in the correct order (second on top).
    assert_ne!(first_only, overlap);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_selected_types_filters_mask_values() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(Some("person"));
    pipeline.set_state(gst::State::Playing).unwrap();

    // Values 1 and 2 map to "person" and "car" respectively through attached classification.
    let mask = make_mask_buffer(4, 4, vec![1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2]);
    let input = make_segmented_buffer(gst::ClockTime::ZERO, vec![(mask, 16, 16, 16, 16)], true);

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);

    let left_person = pixel_bgra(&out, 18, 18);
    let right_car = pixel_bgra(&out, 30, 18);
    // Verify left region (value 1="person", matches filter) is rendered with non-zero alpha.
    // This confirms the element correctly renders mask regions that match selected-types.
    assert!(left_person[3] > 0);
    // Verify right region (value 2="car", does not match filter) has zero alpha.
    // This confirms the element correctly filters out regions that don't match selected-types.
    assert_eq!(right_car[3], 0);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_selected_types_without_classification_still_renders_mask() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(Some("person"));
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask = make_mask_buffer(4, 4, vec![1_u8; 16]);
    let input = make_segmented_buffer(gst::ClockTime::ZERO, vec![(mask, 8, 8, 16, 16)], false);

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    // Verify mask is still rendered even without classification metadata.
    // The element should fall back to rendering all mask regions when selected-types
    // filtering cannot be applied due to missing classification data.
    assert!(count_nonzero_alpha(&out) > 0);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_mask_is_clipped_to_frame_bounds() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask = make_mask_buffer(8, 8, vec![1_u8; 64]);
    let input = make_segmented_buffer(gst::ClockTime::ZERO, vec![(mask, 56, 56, 24, 24)], true);

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    // Verify pixel at (0, 0) has zero alpha. The mask starts at (56, 56) so the
    // top-left corner should not be rendered, confirming clipping to frame bounds.
    assert_eq!(pixel_bgra(&out, 0, 0)[3], 0);
    // Verify pixel at (63, 63) has non-zero alpha. The mask extends into the
    // bottom-right corner of the frame, confirming it is rendered up to the frame edge.
    assert!(pixel_bgra(&out, 63, 63)[3] > 0);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_merges_upstream_composition_with_segmentation_overlay() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let seg_mask = make_mask_buffer(4, 4, vec![1_u8; 16]);
    let mut input =
        make_segmented_buffer(gst::ClockTime::ZERO, vec![(seg_mask, 28, 28, 16, 16)], true);

    {
        let buffer_ref = input.get_mut().unwrap();
        attach_upstream_overlay_rect(buffer_ref, 4, 4, 8, 8, [0x00, 0x00, 0x80, 0x80]);
    }

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    // Verify pixel at (6, 6) has non-zero alpha. This is within the upstream
    // composition rectangle (4, 4, 8, 8), confirming upstream overlays are preserved.
    assert!(pixel_bgra(&out, 6, 6)[3] > 0);
    // Verify pixel at (30, 30) has non-zero alpha. This is within the segmentation
    // mask region (28, 28, 16, 16), confirming segmentation overlay is rendered.
    // This verifies both upstream and element-generated overlays are merged correctly.
    assert!(pixel_bgra(&out, 30, 30)[3] > 0);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_runtime_property_changes_apply_in_playing_and_preserve_segment_color() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask_first = make_mask_buffer(4, 4, vec![1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2]);
    let input_first = make_segmented_buffer(
        gst::ClockTime::from_seconds(0),
        vec![(mask_first, 16, 16, 16, 16)],
        true,
    );

    appsrc.push_buffer(input_first).unwrap();
    let out_first = pull_buffer_from_appsink(&appsink);

    let first_person = pixel_bgra(&out_first, 18, 18);
    let first_car = pixel_bgra(&out_first, 30, 18);
    // Verify both segments render in the first frame (no filtering applied yet).
    assert!(first_person[3] > 0);
    assert!(first_car[3] > 0);

    let overlay = pipeline
        .by_name("overlay")
        .expect("overlay element not found");
    overlay.set_property("hint-maximum-segment-type", 100_u32);
    overlay.set_property("selected-types", "person");

    let mask_second = make_mask_buffer(4, 4, vec![1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2]);
    let input_second = make_segmented_buffer(
        gst::ClockTime::from_seconds(1),
        vec![(mask_second, 16, 16, 16, 16)],
        true,
    );

    appsrc.push_buffer(input_second).unwrap();
    let out_second = pull_buffer_from_appsink(&appsink);

    let second_person = pixel_bgra(&out_second, 18, 18);
    let second_car = pixel_bgra(&out_second, 30, 18);

    // Verify person segment keeps same color assignment as first frame.
    // This ensures color stability when applying property changes at runtime.
    assert_eq!(second_person, first_person);
    // Verify car segment is now filtered out (alpha=0) after selected-types="person" is set.
    // This confirms property changes take effect immediately and don't affect previous color assignments.
    assert_eq!(second_car[3], 0);

    appsrc.end_of_stream().unwrap();
    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_selected_types_with_multiple_n_to_n_classifications_still_filters() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(Some("person"));
    pipeline.set_state(gst::State::Playing).unwrap();

    let mut buffer = gst::Buffer::from_mut_slice(vec![0_u8; BUFFER_SIZE]);
    {
        let buffer_ref = buffer.get_mut().unwrap();
        buffer_ref.set_pts(gst::ClockTime::ZERO);

        let mut relation = gst_analytics::AnalyticsRelationMeta::add(buffer_ref);
        let mask = make_mask_buffer(4, 4, vec![1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2, 1, 1, 2, 2]);
        let seg_id = add_segmentation_mtd(&mut relation, mask, 16, 16, 16, 16);

        let classes_primary = [
            glib::Quark::from_str("background"),
            glib::Quark::from_str("person"),
            glib::Quark::from_str("car"),
            glib::Quark::from_str("dog"),
        ];
        let levels_primary = [1.0_f32, 1.0_f32, 1.0_f32, 1.0_f32];
        let cls_primary_id = {
            let cls = relation
                .add_cls_mtd(&levels_primary, &classes_primary)
                .unwrap();
            cls.id()
        };
        relation
            .set_relation(RelTypes::N_TO_N, seg_id, cls_primary_id)
            .unwrap();

        let classes_secondary = [
            glib::Quark::from_str("background"),
            glib::Quark::from_str("person"),
            glib::Quark::from_str("car"),
            glib::Quark::from_str("tree"),
        ];
        let levels_secondary = [1.0_f32, 1.0_f32, 1.0_f32, 1.0_f32];
        let cls_secondary_id = {
            let cls = relation
                .add_cls_mtd(&levels_secondary, &classes_secondary)
                .unwrap();
            cls.id()
        };
        relation
            .set_relation(RelTypes::N_TO_N, seg_id, cls_secondary_id)
            .unwrap();
    }

    appsrc.push_buffer(buffer).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    let left_person = pixel_bgra(&out, 18, 18);
    let right_car = pixel_bgra(&out, 30, 18);
    // Verify person segment (value 1, matches filter) renders with non-zero alpha.
    // This works even with multiple N_TO_N classifications linked to the segment.
    assert!(left_person[3] > 0);
    // Verify car segment (value 2, doesn't match filter) has zero alpha.
    // This confirms filtering correctly applies even with multiple classification relationships.
    assert_eq!(right_car[3], 0);

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_no_metadata_frame_reuses_previous_overlay() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask = make_mask_buffer(4, 4, vec![1_u8; 16]);
    let input_first = make_segmented_buffer(
        gst::ClockTime::from_seconds(0),
        vec![(mask, 16, 16, 16, 16)],
        true,
    );

    appsrc.push_buffer(input_first).unwrap();
    let out_first = pull_buffer_from_appsink(&appsink);
    let first_overlay_px = pixel_bgra(&out_first, 18, 18);
    // Verify first frame with metadata renders overlay.
    assert!(first_overlay_px[3] > 0);

    let input_second = make_plain_buffer(gst::ClockTime::from_seconds(1));
    appsrc.push_buffer(input_second).unwrap();
    let out_second = pull_buffer_from_appsink(&appsink);

    // Verify second frame without metadata reuses previous overlay.
    // This ensures continuity when metadata is momentarily unavailable.
    let second_overlay_px = pixel_bgra(&out_second, 18, 18);
    assert!(second_overlay_px[3] > 0);

    appsrc.end_of_stream().unwrap();
    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn pipeline_default_caps_blends_and_does_not_attach_overlay_meta() {
    init();

    let (pipeline, appsrc, appsink) = make_pipeline(None);
    pipeline.set_state(gst::State::Playing).unwrap();

    let mask = make_mask_buffer(4, 4, vec![1_u8; 16]);
    let input = make_segmented_buffer(gst::ClockTime::ZERO, vec![(mask, 16, 16, 16, 16)], true);

    appsrc.push_buffer(input).unwrap();
    appsrc.end_of_stream().unwrap();

    let out = pull_buffer_from_appsink(&appsink);
    // Verify overlay is blended directly into the video frame (not as composition metadata).
    // This is the default behavior when downstream doesn't support overlay composition.
    assert!(count_nonzero_alpha(&out) > 0);
    // Verify no VideoOverlayCompositionMeta is attached. Without overlay-meta feature
    // in caps, the element should blend overlays directly into pixel data instead.
    assert!(
        out.iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .next()
            .is_none()
    );

    wait_for_pipeline_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn eos_event_is_accepted() {
    init();

    let mut harness = make_harness(None);

    // Verify the element accepts EOS (End of Stream) events gracefully.
    // This ensures proper stream termination and resource cleanup.
    assert!(harness.push_event(gst::event::Eos::new()));
}

#[test]
fn flush_stop_clears_stale_overlay_and_resumes_processing() {
    init();

    let mut harness = make_harness(None);

    // Push initial buffer with segmentation metadata.
    assert_eq!(
        harness.push(make_segmented_buffer(
            gst::ClockTime::ZERO,
            vec![(make_mask_buffer(4, 4, vec![1_u8; 16]), 16, 16, 16, 16)],
            true,
        )),
        Ok(gst::FlowSuccess::Ok)
    );
    let first = harness.pull().unwrap();
    // Verify overlay is drawn for the segmented frame.
    assert!(buffer_has_drawn_pixels(&first));

    // Initiate flush sequence.
    assert!(harness.push_event(gst::event::FlushStart::new()));
    // Verify element rejects buffer push during flush with Flushing error.
    assert_eq!(
        harness.push(make_plain_buffer(gst::ClockTime::from_mseconds(10))),
        Err(gst::FlowError::Flushing)
    );

    // Complete flush sequence.
    assert!(harness.push_event(gst::event::FlushStop::new(true)));
    push_time_segment_to_harness(&mut harness);

    // Push plain frame (no metadata) after flush. The stale overlay should have been cleared.
    assert_eq!(
        harness.push(make_plain_buffer(gst::ClockTime::from_mseconds(20))),
        Ok(gst::FlowSuccess::Ok)
    );
    let cleared = harness.pull().unwrap();
    // Verify no overlay is drawn. FLUSH_STOP should have cleared cached composition state.
    assert!(!buffer_has_drawn_pixels(&cleared));

    // Push new segmented buffer to confirm processing resumes normally.
    assert_eq!(
        harness.push(make_segmented_buffer(
            gst::ClockTime::from_mseconds(30),
            vec![(make_mask_buffer(4, 4, vec![1_u8; 16]), 16, 16, 16, 16)],
            true,
        )),
        Ok(gst::FlowSuccess::Ok)
    );
    let resumed = harness.pull().unwrap();
    // Verify overlay is drawn again after resuming from flush.
    // This confirms element correctly resets state and continues processing.
    assert!(buffer_has_drawn_pixels(&resumed));
}
