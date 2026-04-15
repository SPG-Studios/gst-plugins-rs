use gst::debug;
use gst::glib;
use gst::prelude::*;

use std::str::FromStr;
use std::sync::LazyLock;

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

fn enable_valve(pipeline: &gst::Pipeline, is_fallback: bool) {
    let valve = pipeline
        .by_name(format!("valve-{}", if is_fallback { "fallback" } else { "main" }).as_str())
        .unwrap();
    valve.set_property("drop", true);
}

fn source_bin(
    num_buffers: i32,
    drop: bool,
    width: i32,
    height: i32,
    is_fallback: bool,
) -> gst::Bin {
    let src = gst::ElementFactory::make("videotestsrc")
        .name("src")
        .property("num-buffers", num_buffers)
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

fn setup_pipeline(
    num_buffers: i32,
    drop_src: bool,
    with_live_fallback: bool,
    immediate_fallback: bool,
) -> gst::Pipeline {
    init();

    debug!(TEST_CAT, "Setting up pipeline");

    let pipeline = gst::Pipeline::default();

    let src = source_bin(num_buffers, drop_src, 320, 240, false);

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
        let fallback_src = source_bin(num_buffers, false, 160, 120, true);
        source.set_property("fallback-source", fallback_src.upcast_ref::<gst::Element>());
    }

    pipeline.set_state(gst::State::Playing).unwrap();
    source.sync_state_with_parent().unwrap();

    pipeline
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

fn stop_pipeline(pipeline: gst::Pipeline) {
    pipeline.set_state(gst::State::Null).unwrap();
}

#[test]
fn test_main_without_fallback() {
    let pipeline = setup_pipeline(4, false, false, false);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    stop_pipeline(pipeline);
}

#[test]
fn test_fallback_without_main() {
    let pipeline = setup_pipeline(4, true, true, true);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);

    stop_pipeline(pipeline);
}

#[test]
fn test_main_with_fallback() {
    let pipeline = setup_pipeline(4, false, true, false);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    stop_pipeline(pipeline);
}

#[test]
fn test_main_with_drops_with_fallback() {
    let pipeline = setup_pipeline(4, false, true, false);

    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer);

    enable_valve(&pipeline, false);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);

    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer);

    stop_pipeline(pipeline);
}
