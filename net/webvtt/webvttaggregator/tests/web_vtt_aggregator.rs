#[cfg(test)]
mod tests {
    use gst::EventType::Eos;
    use gst::prelude::{ObjectExt, ToValue};
    use gst::{ClockTime, Event};
    use gst_check::Harness;
    use std::time::Duration;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
            webvttaggregator::plugin_register_static().unwrap()
        });
    }

    fn push_web_vtt_header(harness: &mut Harness) {
        harness
            .push(make_buffer("WEBVTT\n\n".as_bytes(), None, None))
            .unwrap();
    }

    fn push_cue(harness: &mut Harness, duration: u64, start: u64) {
        let end = start + duration;

        let start_duration = Duration::from_millis(start);
        let end_duration = Duration::from_millis(end);

        let web_vtt = format!(
            "{} --> {}
Test {}",
            format_duration(start_duration),
            format_duration(end_duration),
            format_duration(start_duration)
        );

        harness
            .push(make_buffer(
                web_vtt.as_bytes(),
                Some(gst::ClockTime::from_mseconds(start)),
                Some(gst::ClockTime::from_mseconds(duration)),
            ))
            .unwrap();
    }

    fn make_buffer(
        content: &[u8],
        pts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
    ) -> gst::Buffer {
        let mut buf = gst::Buffer::from_slice(content.to_owned());

        if let Some(pts) = pts {
            buf.make_mut().set_pts(pts);
        }

        if let Some(duration) = duration {
            buf.make_mut().set_duration(duration);
        }

        buf
    }

    fn format_duration(duration: Duration) -> String {
        let millis = duration.as_millis() % 1000;
        let seconds = (duration.as_secs() % 60) as u64;
        let minutes = (duration.as_secs() / 60) as u64;

        format!("{:02}:{:02}.{:03}", minutes, seconds, millis)
    }

    #[test]
    fn test_vtt_perfect_aggregation() -> Result<(), String> {
        init();

        let element = gst::ElementFactory::make("webvttaggregator")
            .build()
            .unwrap();

        element.set_property("target-duration", ClockTime::from_seconds(2).to_value());

        let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
        harness.set_src_caps_str("application/x-subtitle-vtt");
        harness.set_sink_caps_str("application/x-subtitle-vtt");
        harness.use_testclock();

        harness.play();

        push_web_vtt_header(&mut harness);

        let duration = 500u64;
        for start in (0..=2000u64).step_by(duration as usize) {
            push_cue(&mut harness, duration, start);
        }

        harness.crank_single_clock_wait().unwrap();

        let buffer1 = harness.pull().unwrap();

        for start in (2500..=3500u64).step_by(duration as usize) {
            push_cue(&mut harness, duration, start);
        }

        harness.crank_single_clock_wait().unwrap();

        let buffer2 = harness.pull().unwrap();

        let buffer_mapped1 = buffer1
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;
        let buffer_mapped2 = buffer2
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;

        let vtt1 = std::str::from_utf8(buffer_mapped1.as_slice()).unwrap();
        let vtt2 = std::str::from_utf8(buffer_mapped2.as_slice()).unwrap();

        assert_eq!(buffer1.pts().unwrap().mseconds(), 0);
        assert_eq!(buffer1.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt1,
            "WEBVTT

00:00:00.000 --> 00:00:00.500
Test 00:00.000

00:00:00.500 --> 00:00:01.000
Test 00:00.500

00:00:01.000 --> 00:00:01.500
Test 00:01.000

00:00:01.500 --> 00:00:02.000
Test 00:01.500"
        );

        assert_eq!(buffer2.pts().unwrap().mseconds(), 2000);
        assert_eq!(buffer2.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt2,
            "WEBVTT

00:00:02.000 --> 00:00:02.500
Test 00:02.000

00:00:02.500 --> 00:00:03.000
Test 00:02.500

00:00:03.000 --> 00:00:03.500
Test 00:03.000

00:00:03.500 --> 00:00:04.000
Test 00:03.500"
        );

        Ok(())
    }

    #[test]
    fn test_vtt_overlap_aggregation() -> Result<(), String> {
        init();

        let element = gst::ElementFactory::make("webvttaggregator")
            .build()
            .unwrap();

        element.set_property("target-duration", ClockTime::from_seconds(2).to_value());

        let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
        harness.set_src_caps_str("application/x-subtitle-vtt");
        harness.set_sink_caps_str("application/x-subtitle-vtt");
        harness.use_testclock();

        harness.play();

        push_web_vtt_header(&mut harness);

        let duration = 750u64;
        for start in (0..=1500u64).step_by(duration as usize) {
            push_cue(&mut harness, duration, start);
        }

        harness.crank_single_clock_wait().unwrap();

        for start in (2250..=3750u64).step_by(duration as usize) {
            push_cue(&mut harness, duration, start);
        }

        harness.crank_single_clock_wait().unwrap();

        let buffer1 = harness.pull().unwrap();
        let buffer2 = harness.pull().unwrap();

        let buffer_mapped1 = buffer1
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;
        let buffer_mapped2 = buffer2
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;

        let vtt1 = std::str::from_utf8(buffer_mapped1.as_slice()).unwrap();
        let vtt2 = std::str::from_utf8(buffer_mapped2.as_slice()).unwrap();

        assert_eq!(buffer1.pts().unwrap().mseconds(), 0);
        assert_eq!(buffer1.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt1,
            "WEBVTT

00:00:00.000 --> 00:00:00.750
Test 00:00.000

00:00:00.750 --> 00:00:01.500
Test 00:00.750

00:00:01.500 --> 00:00:02.000
Test 00:01.500"
        );

        assert_eq!(buffer2.pts().unwrap().mseconds(), 2000);
        assert_eq!(buffer2.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt2,
            "WEBVTT

00:00:02.000 --> 00:00:02.250
Test 00:01.500

00:00:02.250 --> 00:00:03.000
Test 00:02.250

00:00:03.000 --> 00:00:03.750
Test 00:03.000

00:00:03.750 --> 00:00:04.000
Test 00:03.750"
        );

        Ok(())
    }

    #[test]
    fn test_one_sparse_cue() -> Result<(), String> {
        init();

        let element = gst::ElementFactory::make("webvttaggregator")
            .build()
            .unwrap();

        element.set_property("target-duration", ClockTime::from_seconds(2).to_value());

        let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
        harness.set_src_caps_str("application/x-subtitle-vtt");
        harness.set_sink_caps_str("application/x-subtitle-vtt");
        harness.use_testclock();

        harness.play();

        push_web_vtt_header(&mut harness);
        push_cue(&mut harness, 500, 0);

        harness.set_time(ClockTime::from_mseconds(2100)).unwrap();
        harness.crank_single_clock_wait().unwrap();

        let buffer1 = harness.pull().unwrap();

        harness.set_time(ClockTime::from_mseconds(4100)).unwrap();
        harness.crank_single_clock_wait().unwrap();

        let buffer2 = harness.pull().unwrap();

        let buffer_mapped1 = buffer1
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;

        let vtt1 = std::str::from_utf8(buffer_mapped1.as_slice()).unwrap();

        let buffer_mapped2 = buffer2
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;

        let vtt2 = std::str::from_utf8(buffer_mapped2.as_slice()).unwrap();

        assert_eq!(buffer1.pts().unwrap().mseconds(), 0);
        assert_eq!(buffer1.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt1,
            "WEBVTT

00:00:00.000 --> 00:00:00.500
Test 00:00.000"
        );

        assert_eq!(buffer2.pts().unwrap().mseconds(), 2000);
        assert_eq!(buffer2.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt2,
            "WEBVTT\n\n"
        );

        Ok(())
    }

    #[test]
    fn test_webvttenc_webvttaggregator() -> Result<(), String> {
        init();

        let mut harness =
            gst_check::Harness::new_parse("webvttenc ! webvttaggregator target-duration=2000000000");
        harness.set_src_caps_str("text/x-raw,format=utf8");
        harness.use_testclock();

        harness.play();

        let duration = 500u64;
        for start in (0..=1500u64).step_by(duration as usize) {
            let start_duration = Duration::from_millis(start);

            let plain_text = format!("Test {}", format_duration(start_duration));

            harness
                .push(make_buffer(
                    plain_text.as_bytes(),
                    Some(gst::ClockTime::from_mseconds(start)),
                    Some(gst::ClockTime::from_mseconds(duration)),
                ))
                .unwrap();
        }

        harness.set_time(ClockTime::from_mseconds(2100)).unwrap();
        harness.crank_single_clock_wait().unwrap();

        let buffer1 = harness.pull().unwrap();

        for start in (2000..=3500u64).step_by(duration as usize) {
            let start_duration = Duration::from_millis(start);

            let plain_text = format!("Test {}", format_duration(start_duration));

            harness
                .push(make_buffer(
                    plain_text.as_bytes(),
                    Some(gst::ClockTime::from_mseconds(start)),
                    Some(gst::ClockTime::from_mseconds(duration)),
                ))
                .unwrap();
        }

        harness.set_time(ClockTime::from_mseconds(4100)).unwrap();
        harness.crank_single_clock_wait().unwrap();

        let buffer2 = harness.pull().unwrap();

        let buffer_mapped1 = buffer1
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;
        let buffer_mapped2 = buffer2
            .map_readable()
            .map_err(|e| format!("Error mapping output buffer: {e}"))?;

        let vtt1 = std::str::from_utf8(buffer_mapped1.as_slice()).unwrap();
        let vtt2 = std::str::from_utf8(buffer_mapped2.as_slice()).unwrap();

        assert_eq!(buffer1.pts().unwrap().mseconds(), 0);
        assert_eq!(buffer1.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt1,
            "WEBVTT

00:00:00.000 --> 00:00:00.500
Test 00:00.000

00:00:00.500 --> 00:00:01.000
Test 00:00.500

00:00:01.000 --> 00:00:01.500
Test 00:01.000

00:00:01.500 --> 00:00:02.000
Test 00:01.500"
        );

        assert_eq!(buffer2.pts().unwrap().mseconds(), 2000);
        assert_eq!(buffer2.duration().unwrap().mseconds(), 2000);
        assert_eq!(
            vtt2,
            "WEBVTT

00:00:02.000 --> 00:00:02.500
Test 00:02.000

00:00:02.500 --> 00:00:03.000
Test 00:02.500

00:00:03.000 --> 00:00:03.500
Test 00:03.000

00:00:03.500 --> 00:00:04.000
Test 00:03.500"
        );

        Ok(())
    }
}
