use clap::Parser;
use gst::glib;
use gst::prelude::*;
use std::sync::LazyLock;
use url::Url;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "gst-moq-sub",
        gst::DebugColorFlags::empty(),
        Some("gst-moq-sub"),
    )
});

static DEFAULT_MOQ_ALPN: &str = "moq-00";
static DEFAULT_MOQ_SCHEME_HTTPS: &str = "https";
static DEFAULT_MOQ_SCHEME: &str = "moqt";
static DEFAULT_MOQ_RELAY_ADDR: &str = "127.0.0.1";
static DEFAULT_MOQ_RELAY_PORT: u16 = 4443;

static DEFAULT_SERVER_NAME: &str = "localhost";
static DEFAULT_BIND_ADDR: &str = "0.0.0.0";
static DEFAULT_BIND_PORT: u16 = 0;
static DEFAULT_MOQ_NAMESPACE: &str = "bbb";

#[derive(Parser, Debug)]
#[command(name = "gst-moq-sub")]
#[command(version = "0.1")]
#[command(about = "Code for testing Media over QUIC", long_about = None)]
struct Cli {
    #[clap(long, short, action)]
    webtransport: bool,
}

fn create_sink_bin(is_audio: bool) -> gst::Bin {
    let bin = if is_audio {
        gst::Bin::builder().name("audio-sink-bin").build()
    } else {
        gst::Bin::builder().name("video-sink-bin").build()
    };

    let convert = if is_audio {
        gst::ElementFactory::make("audioconvert").build().unwrap()
    } else {
        gst::ElementFactory::make("videoconvert").build().unwrap()
    };

    let sink = if is_audio {
        gst::ElementFactory::make("autoaudiosink").build().unwrap()
    } else {
        gst::ElementFactory::make("autovideosink").build().unwrap()
    };

    let queue = gst::ElementFactory::make("queue").build().unwrap();
    let queue2 = gst::ElementFactory::make("queue").build().unwrap();

    for q in [&queue, &queue2] {
        q.set_property_from_str("leaky", "downstream");
        q.set_property("max-size-buffers", 0u32);
        q.set_property("max-size-bytes", 0u32);
        q.set_property("max-size-time", gst::ClockTime::from_mseconds(500));
    }

    bin.add_many([&queue, &convert, &queue2, &sink]).unwrap();
    gst::Element::link_many([&queue, &convert, &queue2, &sink]).unwrap();

    let sinkpad = queue.static_pad("sink").unwrap();
    let ghost_pad = gst::GhostPad::builder(gst::PadDirection::Sink)
        .name("sink")
        .build();
    ghost_pad.set_target(Some(&sinkpad)).unwrap();

    bin.add_pad(&ghost_pad).unwrap();

    bin
}

fn main() {
    let cli = Cli::parse();

    gst::init().unwrap();

    let pipeline = gst::Pipeline::new();
    let main_loop = glib::MainLoop::new(None, false);

    let quicsrc = if cli.webtransport {
        gst::ElementFactory::make("quinnwtsrc")
            .name("moq-src")
            .build()
            .unwrap()
    } else {
        gst::ElementFactory::make("quinnquicsrc")
            .name("moq-src")
            .build()
            .unwrap()
    };

    let demux = gst::ElementFactory::make("moqdemux")
        .name("moq-demux")
        .build()
        .unwrap();

    let decodebin = gst::ElementFactory::make("decodebin3")
        .name("decodebin3")
        .build()
        .unwrap();
    decodebin.connect_pad_added(glib::clone!(
        #[weak]
        pipeline,
        move |_, pad| {
            if pad.name().contains("sink") {
                return;
            }

            let caps = pad
                .stream()
                .and_then(|stream| stream.caps())
                .expect("Expect stream caps to be valid here");

            gst::info!(CAT, "decodebin {} added with caps {caps:?}", pad.name());

            pipeline.debug_to_dot_file_with_ts(
                gst::DebugGraphDetails::all(),
                "moq-sub-decodebin-pad-added",
            );

            let s = caps.structure(0).unwrap();

            let is_audio = !s.name().contains("video");
            let sink_bin = create_sink_bin(is_audio);

            pipeline.add(&sink_bin).unwrap();
            sink_bin.sync_state_with_parent().unwrap();

            let sinkpad = sink_bin.static_pad("sink").unwrap();
            pad.link(&sinkpad).unwrap();
        }
    ));

    let moq_scheme = if cli.webtransport {
        DEFAULT_MOQ_SCHEME_HTTPS
    } else {
        DEFAULT_MOQ_SCHEME
    };

    let relay_url = Url::parse(
        format!(
            "{}://{}:{}/{}",
            moq_scheme, DEFAULT_MOQ_RELAY_ADDR, DEFAULT_MOQ_RELAY_PORT, DEFAULT_MOQ_NAMESPACE
        )
        .as_str(),
    )
    .unwrap();

    if !cli.webtransport {
        let address = relay_url.host().unwrap();
        let port = relay_url.port().unwrap();
        quicsrc.set_property("address", address.to_string());
        quicsrc.set_property("port", port as u32);

        quicsrc.set_property("server-name", DEFAULT_SERVER_NAME);
        quicsrc.set_property("bind-address", DEFAULT_BIND_ADDR);
        quicsrc.set_property("bind-port", DEFAULT_BIND_PORT as u32);
        quicsrc.set_property_from_str("role", "client");

        let alpns = vec![DEFAULT_MOQ_ALPN.to_string()];
        let alpn_protocols = gst::Array::new(alpns).to_value();
        quicsrc.set_property("alpn-protocols", &alpn_protocols);
    } else {
        quicsrc.set_property_from_str("role", "client");
        quicsrc.set_property("url", relay_url.to_string());
        demux.set_property("url", relay_url.to_string());
    };

    quicsrc.set_property("secure-connection", false);

    pipeline.add_many([&quicsrc, &demux, &decodebin]).unwrap();

    quicsrc.link(&demux).unwrap();

    demux.connect_pad_added(glib::clone!(
        #[weak]
        pipeline,
        move |_, pad| {
            gst::info!(CAT, "MoQ demuxer pad {} added", pad.name());

            let queue = gst::ElementFactory::make("queue")
                .property_from_str("leaky", "downstream")
                .property("max-size-buffers", 0u32)
                .property("max-size-bytes", 0u32)
                .property("max-size-time", 10 * gst::ClockTime::SECOND)
                .build()
                .unwrap();

            pipeline.add(&queue).unwrap();

            let decodebin = pipeline.by_name("decodebin3").unwrap();
            let decodebin_sinkpad = decodebin.request_pad_simple("sink_%u").unwrap();

            let queue_srcpad = queue.static_pad("src").unwrap();
            queue_srcpad.link(&decodebin_sinkpad).unwrap();

            let queue_sinkpad = queue.static_pad("sink").unwrap();
            pad.link(&queue_sinkpad).unwrap();

            queue.sync_state_with_parent().unwrap();

            pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "demux-pad-added");
        }
    ));

    demux.connect_no_more_pads(glib::clone!(
        #[weak]
        pipeline,
        move |_| {
            pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "demux-no-more-pads");
        }
    ));

    let bus = pipeline.bus().unwrap();
    let l_clone = main_loop.clone();
    let _bus_watch = bus
        .add_watch({
            let pipeline_weak = pipeline.downgrade();
            move |_, msg| {
                let pipeline = match pipeline_weak.upgrade() {
                    Some(pipeline) => pipeline,
                    None => return glib::ControlFlow::Break,
                };
                use gst::MessageView;

                match msg.view() {
                    MessageView::Eos(..) => {
                        gst::info!(CAT, "End of stream");
                        l_clone.quit();
                        return glib::ControlFlow::Break;
                    }
                    MessageView::Error(err) => {
                        gst::error!(
                            CAT,
                            "Error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        );
                        l_clone.quit();
                        return glib::ControlFlow::Break;
                    }
                    MessageView::StateChanged(state)
                        if state
                            .src()
                            .map(|s| s == pipeline.upcast_ref::<gst::Object>())
                            .unwrap_or(false) =>
                    {
                        gst::info!(
                            CAT,
                            "Pipeline state changed from {:?} to {:?}",
                            state.old(),
                            state.current()
                        );
                    }
                    MessageView::Latency(_) => {
                        let _ = pipeline.recalculate_latency();
                    }
                    _ => (),
                }
                glib::ControlFlow::Continue
            }
        })
        .unwrap();

    ctrlc::set_handler(glib::clone!(
        #[weak]
        pipeline,
        move || {
            gst::info!(CAT, "Received interrupt, stopping pipeline...");
            pipeline.debug_to_dot_file_with_ts(
                gst::DebugGraphDetails::all(),
                "moq-subscriber-quitting",
            );
            pipeline.send_event(gst::event::Eos::new());
        }
    ))
    .unwrap();

    glib::timeout_add_seconds_once(
        5,
        glib::clone!(
            #[weak]
            pipeline,
            move || {
                pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "moq-subscriber");
            }
        ),
    );

    let _ = pipeline.set_state(gst::State::Playing);

    main_loop.run();

    let _ = pipeline.set_state(gst::State::Null);
}
