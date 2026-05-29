use gst::debug;
use gst::glib;
use gst::prelude::*;

use std::str::FromStr;
use std::sync::LazyLock;
use std::thread;

static TEST_CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "fallbacksrc-test",
        gst::DebugColorFlags::empty(),
        Some("fallbacksrc test"),
    )
});

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstfallbackswitch::plugin_register_static().expect("gstfallbacksrc test");
    });
}

macro_rules! assert_fallback_buffer {
    ($buffer:expr) => {
        assert_eq!($buffer.size(), 160 * 120 * 4);
    };
}

macro_rules! assert_buffer {
    ($buffer:expr) => {
        assert_eq!($buffer.size(), 320 * 240 * 4);
    };
}

struct Pipeline {
    pipeline: gst::Pipeline,
    clock_join_handle: Option<thread::JoinHandle<()>>,
}

impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

fn set_time(pipeline: &Pipeline, time: gst::ClockTime) {
    let clock = pipeline
        .clock()
        .unwrap()
        .downcast::<gst_check::TestClock>()
        .unwrap();

    debug!(TEST_CAT, "Setting time to {}", time);

    clock.set_time(gst::ClockTime::SECOND + time);
}

fn enable_valve(pipeline: &gst::Pipeline, is_fallback: bool) {
    let valve = pipeline
        .by_name(format!("valve-{}", if is_fallback { "fallback" } else { "main" }).as_str())
        .unwrap();
    valve.set_property("drop", true);
}

fn source_bin(drop: bool, width: i32, height: i32, is_fallback: bool) -> gst::Bin {
    let src = gst::ElementFactory::make("videotestsrc")
        .name("src")
        .property("is-live", true)
        .build()
        .unwrap();

    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::from_str(
                format!("video/x-raw,format=ARGB,width={width},height={height}").as_str(),
            )
            .unwrap(),
        )
        .build()
        .unwrap();

    let valve = gst::ElementFactory::make("valve")
        .name(format!("valve-{}", if is_fallback { "fallback" } else { "main" }).as_str())
        .property("drop", drop)
        .build()
        .unwrap();

    let bin = gst::Bin::with_name(
        format!("src-{}", if is_fallback { "fallback" } else { "main" }).as_str(),
    );

    bin.add(&src).unwrap();
    bin.add(&caps).unwrap();
    bin.add(&valve).unwrap();

    gst::Element::link_many([&src, &caps, &valve]).unwrap();

    let srcpad = valve.static_pad("src").unwrap();

    let srcpad = gst::GhostPad::builder(gst::PadDirection::Src)
        .with_target(&srcpad)
        .unwrap()
        .name("src")
        .build();
    bin.add_pad(&srcpad).unwrap();

    bin
}

fn setup_pipeline(drop_src: bool, with_live_fallback: bool, immediate_fallback: bool) -> Pipeline {
    init();

    debug!(TEST_CAT, "Setting up pipeline");

    let clock = gst_check::TestClock::new();
    clock.set_time(gst::ClockTime::ZERO);
    let pipeline = gst::Pipeline::default();

    // Running time 0 in our pipeline is going to be clock time 1s. All
    // clock ids before 1s are used for signalling to our clock advancing
    // thread.
    pipeline.use_clock(Some(&clock));
    pipeline.set_base_time(gst::ClockTime::SECOND);
    pipeline.set_start_time(gst::ClockTime::NONE);

    let src = source_bin(drop_src, 320, 240, false);

    let source = gst::ElementFactory::make("fallbacksrc")
        .name("source")
        .property("immediate-fallback", immediate_fallback)
        .property("source", src)
        .property("enable-dummy", false)
        .property("enable-audio", false)
        .build()
        .unwrap();

    source.connect_pad_added(glib::clone!(
        #[weak]
        pipeline,
        move |_, src_pad| {
            let caps = src_pad
                .stream()
                .and_then(|stream| stream.caps())
                .expect("Expect stream caps to be valid here");

            debug!(TEST_CAT, "{} added with caps {caps:?}", src_pad.name());

            let sink = gst_app::AppSink::builder().sync(false).name("sink").build();
            pipeline.add(&sink).unwrap();

            let sink_pad = sink.static_pad("sink").unwrap();

            src_pad.link(&sink_pad).unwrap();
            sink.sync_state_with_parent().unwrap();
        }
    ));

    pipeline.add(&source).unwrap();

    if with_live_fallback {
        let fallback_src = source_bin(false, 160, 120, true);
        source.set_property("fallback-source", fallback_src.upcast_ref::<gst::Element>());
    }

    pipeline.set_state(gst::State::Playing).unwrap();
    source.sync_state_with_parent().unwrap();

    let clock_join_handle = thread::spawn(move || {
        loop {
            while let Some(clock_id) = clock.peek_next_pending_id().and_then(|clock_id| {
                // Process if the clock ID is in the past or now
                if clock.time() >= clock_id.time() {
                    Some(clock_id)
                } else {
                    None
                }
            }) {
                debug!(
                    TEST_CAT,
                    "Processing clock ID {} at {:?}",
                    clock_id.time(),
                    clock.time()
                );
                if let Some(clock_id) = clock.process_next_clock_id() {
                    debug!(TEST_CAT, "Processed clock ID {}", clock_id.time());
                    if clock_id.time().is_zero() {
                        debug!(TEST_CAT, "Stopping clock thread");
                        return;
                    }
                }
            }

            // Sleep for 5ms as long as we have pending clock IDs that
            // are in the future at the top of the queue. We don't want
            // to do a busy loop here.
            while clock.peek_next_pending_id().iter().any(|clock_id| {
                // Sleep if the clock ID is in the future
                clock.time() < clock_id.time()
            }) {
                thread::sleep(std::time::Duration::from_millis(10));
            }

            // Otherwise if there are none (or they are ready now) wait
            // until there are clock ids again.
            let _ = clock.wait_for_next_pending_id();
        }
    });

    Pipeline {
        pipeline,
        clock_join_handle: Some(clock_join_handle),
    }
}

fn pull_buffer(pipeline: &gst::Pipeline) -> gst::Buffer {
    let sink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let sample = sink.pull_sample().unwrap();
    sample.buffer_owned().unwrap()
}

fn stop_pipeline(mut pipeline: Pipeline) {
    pipeline.set_state(gst::State::Null).unwrap();

    let clock = pipeline
        .clock()
        .unwrap()
        .downcast::<gst_check::TestClock>()
        .unwrap();

    // Signal shutdown to the clock thread
    let clock_id = clock.new_single_shot_id(gst::ClockTime::ZERO);
    let _ = clock_id.wait();

    pipeline.clock_join_handle.take().unwrap().join().unwrap();
}

#[test]
fn test_main_without_fallback() {
    let pipeline = setup_pipeline(false, false, false);
    set_time(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    set_time(&pipeline, 1.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    set_time(&pipeline, 2.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    set_time(&pipeline, 3.seconds());

    stop_pipeline(pipeline);
}

#[test]
fn test_fallback_without_main() {
    let pipeline = setup_pipeline(true, true, true);
    set_time(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 1.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 2.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 3.seconds());

    stop_pipeline(pipeline);
}

#[test]
fn test_main_with_fallback() {
    let pipeline = setup_pipeline(false, true, false);
    set_time(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    set_time(&pipeline, 1.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    set_time(&pipeline, 2.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);
    set_time(&pipeline, 3.seconds());

    stop_pipeline(pipeline);
}

#[test]
fn test_main_with_drops_with_fallback() {
    let pipeline = setup_pipeline(false, true, false);
    set_time(&pipeline, gst::ClockTime::ZERO);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    enable_valve(&pipeline, false);
    // Default `timeout` for switching to the fallback URI is 5s.
    set_time(&pipeline, 7.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 8.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 9.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 10.seconds());

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);
    set_time(&pipeline, 11.seconds());

    stop_pipeline(pipeline);
}
