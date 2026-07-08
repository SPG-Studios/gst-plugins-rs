// Copyright (C) 2026 Collabora Ltd
//   Author: Olivier Crête <olivier.crete@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
//

pub mod support;

#[cfg(feature = "v1_28")]
mod gimitest {

    use tempfile::tempdir;

    use crate::support::{ExpectedConfiguration, check_generic_single_trak_file_structure, init};

    use gst::prelude::*;

    const GIMI_CONTENT_ID: &str = "urn:uuid:00000000-0000-0000-0000-000000000000";

    #[test]
    fn test_gimi_video_h264() {
        init();

        let video_enc = "x264enc speed-preset=ultrafast";

        let filename = format!("gimi_{video_enc}.mp4").to_string();
        let temp_dir = tempdir().unwrap();
        let temp_file_path = temp_dir.path().join(filename);
        let location = temp_file_path.as_path();
        let pipeline_text = format!(
            "gimimp4mux name=m \
	     ! filesink location={location:?} videotestsrc num-buffers=250 \
	     ! {video_enc} ! m.sink_video_0"
        );
        let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
            println!("could not build encoding pipeline");
            return;
        };
        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
        for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");

        check_generic_single_trak_file_structure(
            location,
            b"iso4".into(),
            0,
            vec![
                b"iso4".into(),
                b"iso6".into(),
                b"isom".into(),
                b"mp41".into(),
                b"mp42".into(),
                b"geo1".into(),
                b"unif".into(),
            ],
            ExpectedConfiguration {
                has_ctts: false,
                has_stss: true,
                has_taic: true,
                taic_time_uncertainty: 0xFFFF_FFFF_FFFF_FFFF,
                taic_clock_type: 0,
                num_tai_chunks: 1,
                num_tai_timestamps: 250,
                num_suid_chunks: 1,
                num_suid_entries: 250,
                gimi_track_content_id: Some(GIMI_CONTENT_ID),
                gimi_component_content_ids: &[GIMI_CONTENT_ID, GIMI_CONTENT_ID, GIMI_CONTENT_ID],
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_gimi_images_h265() {
        init();

        let filename = "gimi_x265.mp4";
        let temp_dir = tempdir().unwrap();
        let temp_file_path = temp_dir.path().join(filename);
        let location = temp_file_path.as_path();
        let pipeline_text = format!(
            "gimimp4mux name=m \
	     ! filesink location={location:?} videotestsrc num-buffers=25 \
	     ! x265enc speed-preset=ultrafast ! h265parse ! m.sink_image_0"
        );
        let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
            println!("could not build encoding pipeline");
            return;
        };
        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
        for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");

        check_generic_single_trak_file_structure(
            location,
            b"msf1".into(),
            0,
            vec![
                b"iso8".into(),
                b"iso6".into(),
                b"msf1".into(),
                b"unif".into(),
                b"geo1".into(),
            ],
            ExpectedConfiguration {
                has_taic: true,
                taic_time_uncertainty: 0xFFFF_FFFF_FFFF_FFFF,
                taic_clock_type: 0,
                num_tai_chunks: 1,
                num_tai_timestamps: 25,
                num_suid_chunks: 1,
                num_suid_entries: 25,
                gimi_track_content_id: Some(GIMI_CONTENT_ID),
                gimi_component_content_ids: &[GIMI_CONTENT_ID, GIMI_CONTENT_ID, GIMI_CONTENT_ID],
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_gimi_images_uncompressed_gray() {
        init();

        let filename = "gimi_gray.mp4";
        let temp_dir = tempdir().unwrap();
        let temp_file_path = temp_dir.path().join(filename);
        let location = temp_file_path.as_path();
        let pipeline_text = format!(
            "gimimp4mux name=m \
	     ! filesink location={location:?} videotestsrc num-buffers=25 \
	     ! video/x-raw, format=GRAY8, width=160, height=120 \
	     ! m.sink_image_0"
        );
        let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
            println!("could not build encoding pipeline");
            return;
        };
        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
        for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");

        check_generic_single_trak_file_structure(
            location,
            b"msf1".into(),
            0,
            vec![
                b"iso8".into(),
                b"iso6".into(),
                b"msf1".into(),
                b"unif".into(),
                b"geo1".into(),
            ],
            ExpectedConfiguration {
                has_taic: true,
                taic_time_uncertainty: 0xFFFF_FFFF_FFFF_FFFF,
                taic_clock_type: 0,
                num_tai_chunks: 1,
                num_tai_timestamps: 25,
                num_suid_chunks: 1,
                num_suid_entries: 25,
                gimi_track_content_id: Some(GIMI_CONTENT_ID),
                gimi_component_content_ids: &[GIMI_CONTENT_ID],
                width: 160,
                height: 120,
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_gimi_images_content_id_tags() {
        init();

        const TRACK_ID: &str = "urn:uuid:893634ab-8148-4bbe-8a4f-a237f0cfa6c2";
        const COMP_0_ID: &str = "urn:uuid:b261ed7a-8b54-4a16-802c-58f285298b1e";
        const COMP_1_ID: &str = "urn:uuid:cc5376fe-396b-4bda-a0fc-b6b8eff6b672";
        const COMP_2_ID: &str = "urn:uuid:a2b499c6-0636-4401-b93f-f824ac2e7612";

        let filename = "gimi_tags.mp4";
        let temp_dir = tempdir().unwrap();
        let temp_file_path = temp_dir.path().join(filename);
        let location = temp_file_path.as_path();
        let pipeline_text = format!(
            "gimimp4mux name=m \
	     ! filesink location={location:?} videotestsrc num-buffers=25 \
	     ! taginject tags=\"gimi-track-content-id={TRACK_ID}, \
	     gimi-component-content-id=(GstStructure)\\\"ids,0=(string){COMP_0_ID}, \
	     1=(string){COMP_1_ID}, 2=(string){COMP_2_ID}\\\" \" \
	     ! video/x-raw, format=RGBx, width=160, height=120 \
	     ! m.sink_image_0"
        );
        let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
            println!("could not build encoding pipeline");
            return;
        };
        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
        for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");

        check_generic_single_trak_file_structure(
            location,
            b"msf1".into(),
            0,
            vec![
                b"iso8".into(),
                b"iso6".into(),
                b"msf1".into(),
                b"unif".into(),
                b"geo1".into(),
            ],
            ExpectedConfiguration {
                has_taic: true,
                taic_time_uncertainty: 0xFFFF_FFFF_FFFF_FFFF,
                taic_clock_type: 0,
                num_tai_chunks: 1,
                num_tai_timestamps: 25,
                num_suid_chunks: 1,
                num_suid_entries: 25,
                gimi_track_content_id: Some(TRACK_ID),
                gimi_component_content_ids: &[COMP_0_ID, COMP_1_ID, COMP_2_ID],
                check_gimi_cid: true,
                width: 160,
                height: 120,
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_gimi_security_xml() {
        init();

        const GIMI_XML: &str = "not-really-xml";

        let filename = "gimi_tags.mp4";
        let temp_dir = tempdir().unwrap();
        let temp_file_path = temp_dir.path().join(filename);
        let location = temp_file_path.as_path();
        let pipeline_text = format!(
            "gimimp4mux name=m \
	     ! filesink location={location:?} videotestsrc num-buffers=25 \
	     ! taginject scope=global tags=\"gimi-security-markings-xml=\\\"{}\\\"\" \
	     ! video/x-raw, format=GRAY8, width=160, height=120 \
	     ! m.sink_image_0",
            GIMI_XML
        );
        let Ok(pipeline) = gst::parse::launch(&pipeline_text) else {
            println!("could not build encoding pipeline");
            return;
        };
        pipeline
            .set_state(gst::State::Playing)
            .expect("Unable to set the pipeline to the `Playing` state");
        for msg in pipeline.bus().unwrap().iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;

            match msg.view() {
                MessageView::Eos(..) => break,
                MessageView::Error(err) => {
                    panic!(
                        "Error from {:?}: {} ({:?})",
                        err.src().map(|s| s.path_string()),
                        err.error(),
                        err.debug()
                    );
                }
                _ => (),
            }
        }
        pipeline
            .set_state(gst::State::Null)
            .expect("Unable to set the pipeline to the `Null` state");

        check_generic_single_trak_file_structure(
            location,
            b"msf1".into(),
            0,
            vec![
                b"iso8".into(),
                b"iso6".into(),
                b"msf1".into(),
                b"unif".into(),
                b"geo1".into(),
                b"sm01".into(),
            ],
            ExpectedConfiguration {
                has_taic: true,
                taic_time_uncertainty: 0xFFFF_FFFF_FFFF_FFFF,
                taic_clock_type: 0,
                num_tai_chunks: 1,
                num_tai_timestamps: 25,
                num_suid_chunks: 1,
                num_suid_entries: 25,
                gimi_track_content_id: Some(GIMI_CONTENT_ID),
                gimi_component_content_ids: &[GIMI_CONTENT_ID],
                width: 160,
                height: 120,
                gimi_security_markings_xml: Some(GIMI_XML),
                gimi_security_markings_content_id: Some(GIMI_CONTENT_ID),
                ..Default::default()
            },
        );
    }
}
