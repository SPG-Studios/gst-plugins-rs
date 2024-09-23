use clap::Parser;
use gst::glib;
use gst::prelude::*;
use std::sync::LazyLock;
use url::Url;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "gst-moq-pub",
        gst::DebugColorFlags::empty(),
        Some("gst-moq-pub"),
    )
});

static DEFAULT_MOQ_ALPN: &str = "moq-00";
static DEFAULT_MOQ_SCHEME_HTTPS: &str = "https";
static DEFAULT_MOQ_SCHEME: &str = "moqt";
static DEFAULT_MOQ_RELAY_ADDR: &str = "127.0.0.1";
static DEFAULT_MOQ_RELAY_PORT: u16 = 4443;

static DEFAULT_SERVER_NAME: &str = "localhost";
static DEFAULT_MOQ_NAMESPACE: &str = "bbb";

/// MoQ video publisher example that publishes from a URI
#[derive(Parser, Debug)]
#[command(name = "gst-moq-pub")]
#[command(version = "0.1")]
#[command(about = "Code for testing Media over QUIC", long_about = None)]
struct Cli {
    /// URI
    #[arg(short, long)]
    uri: String,

    /// Fragment duration in milliseconds
    #[arg(short, long, default_value_t = 2000)]
    fragment_duration: u64,

    /// Skip audio from source
    #[arg(long, default_value_t = false)]
    no_audio: bool,

    #[clap(long, short, action, default_value_t = false)]
    webtransport: bool,
}

fn create_enc_bin(
    pipeline: &gst::Pipeline,
    converter: gst::Element,
    rate_adjuster: gst::Element,
    encoder: gst::Element,
    parser: Option<gst::Element>,
    capsfilter: gst::Element,
) -> gst::Bin {
    let bin = gst::Bin::new();

    let queue_in = gst::ElementFactory::make("queue").build().unwrap();
    let queue_out = gst::ElementFactory::make("queue").build().unwrap();

    let mut elements: Vec<&gst::Element> =
        vec![&queue_in, &converter, &rate_adjuster, &encoder, &queue_out];

    if let Some(ref p) = parser {
        elements.push(p);
    }
    elements.push(&capsfilter);

    pipeline.add_many(&elements).unwrap();

    let mut links: Vec<&gst::Element> =
        vec![&queue_in, &converter, &rate_adjuster, &encoder, &queue_out];

    if let Some(ref p) = parser {
        links.push(p);
    }
    links.push(&capsfilter);

    gst::Element::link_many(&links).unwrap();

    let queue_sinkpad = queue_in.static_pad("sink").unwrap();
    let sinkpad = gst::GhostPad::builder(gst::PadDirection::Sink)
        .name("sink")
        .build();
    sinkpad.set_target(Some(&queue_sinkpad)).unwrap();

    let capsfilter_srcpad = capsfilter.static_pad("src").unwrap();
    let srcpad = gst::GhostPad::builder(gst::PadDirection::Src)
        .name("src")
        .build();
    srcpad.set_target(Some(&capsfilter_srcpad)).unwrap();

    bin.add_pad(&sinkpad).unwrap();
    bin.add_pad(&srcpad).unwrap();

    bin
}

fn video_enc_bin(pipeline: &gst::Pipeline) -> gst::Bin {
    let videoconvert = gst::ElementFactory::make("videoconvert").build().unwrap();
    let videorate = gst::ElementFactory::make("videorate").build().unwrap();
    let videoenc = gst::ElementFactory::make("x264enc")
        .property("bframes", 0u32)
        .property("key-int-max", 50u32)
        .property_from_str("tune", "zerolatency")
        .build()
        .unwrap();
    let videoparser = gst::ElementFactory::make("h264parse")
        .property("config-interval", -1)
        .build()
        .unwrap();
    let h264_capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("profile", "main")
                .field("framerate", gst::Fraction::new(25, 1))
                .build(),
        )
        .build()
        .unwrap();

    create_enc_bin(
        pipeline,
        videoconvert,
        videorate,
        videoenc,
        Some(videoparser),
        h264_capsfilter,
    )
}

fn audio_enc_bin(pipeline: &gst::Pipeline) -> gst::Bin {
    let audioconvert = gst::ElementFactory::make("audioconvert").build().unwrap();
    let audioresample = gst::ElementFactory::make("audioresample").build().unwrap();
    let audioenc = gst::ElementFactory::make("avenc_aac").build().unwrap();
    let audio_capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4i32)
                .build(),
        )
        .build()
        .unwrap();

    create_enc_bin(
        pipeline,
        audioconvert,
        audioresample,
        audioenc,
        None,
        audio_capsfilter,
    )
}

fn setup_publisher_pipeline(
    pipeline: &gst::Pipeline,
    moqmux: &gst::Element,
    moqsink: &gst::Element,
    uri: &str,
    no_audio: bool,
) {
    let decodebin = gst::ElementFactory::make("uridecodebin").build().unwrap();
    decodebin.set_property("uri", uri);
    pipeline.add(&decodebin).unwrap();

    let videobin = video_enc_bin(pipeline);

    pipeline
        .add_many([&videobin.clone().upcast(), moqmux, moqsink])
        .unwrap();

    moqmux.link(moqsink).unwrap();

    let video_pad = moqmux.request_pad_simple("sink_%u").unwrap();
    let video_settings = gst::Structure::builder("video-rendition")
        .field("track-name", "video")
        .field("priority", 127u8)
        .build();
    video_pad.set_property("track-settings", &video_settings);

    let video_src_pad = videobin.static_pad("src").unwrap();
    video_src_pad.link(&video_pad).unwrap();

    let audiobin = if !no_audio {
        let audio_pad = moqmux.request_pad_simple("sink_%u").unwrap();
        let audio_settings = gst::Structure::builder("audio-rendition")
            .field("track-name", "audio")
            .field("priority", 100u8)
            .build();
        audio_pad.set_property("track-settings", &audio_settings);

        let audiobin = audio_enc_bin(pipeline);
        pipeline.add(&audiobin).unwrap();

        let audio_src_pad = audiobin.static_pad("src").unwrap();
        audio_src_pad.link(&audio_pad).unwrap();

        Some(audiobin)
    } else {
        None
    };

    decodebin.connect_pad_added(move |_, src_pad| {
        gst::info!(CAT, "{} added on decodebin", src_pad.name());

        let caps = src_pad.current_caps().unwrap();
        let str_caps = caps.structure(0).unwrap();
        let media_type = str_caps.name();

        match media_type.as_str() {
            "video/x-raw" => {
                let sink_pad = videobin.static_pad("sink").unwrap();
                if let Err(err) = src_pad.link(&sink_pad) {
                    gst::error!(CAT, "Failed to link video pad {err:?}");
                }
            }
            "audio/x-raw" => {
                if !no_audio && let Some(audiobin) = &audiobin {
                    let sink_pad = audiobin.static_pad("sink").unwrap();
                    if let Err(err) = src_pad.link(&sink_pad) {
                        gst::error!(CAT, "Failed to link audio pad {err:?}");
                    }
                }
            }
            _ => {
                gst::error!(CAT, "Unknown pad type: {media_type}");
            }
        }
    });
}

fn main() {
    let cli = Cli::parse();

    gst::init().unwrap();

    let pipeline = gst::Pipeline::new();
    let main_loop = glib::MainLoop::new(None, false);

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

    let moqmux = gst::ElementFactory::make("moqmux")
        .property("url", relay_url.as_str())
        .property("namespace", DEFAULT_MOQ_NAMESPACE)
        .property("fragment-duration", cli.fragment_duration.mseconds())
        .property("chunk-duration", 40.mseconds()) // 25fps thus 40ms per frame
        .build()
        .unwrap();

    let sink = if !cli.webtransport {
        let alpns = vec![DEFAULT_MOQ_ALPN.to_string()];
        let alpn_protocols = gst::Array::new(alpns).to_value();

        let relay_address = relay_url.host().unwrap().to_string();
        let relay_port = relay_url.port().unwrap() as u32;

        gst::ElementFactory::make("quinnquicsink")
            .name("moq-sink")
            .property("address", relay_address)
            .property("port", relay_port)
            .property("server-name", DEFAULT_SERVER_NAME)
            .property("alpn-protocols", &alpn_protocols)
            .property_from_str("role", "client")
            .property("secure-connection", false)
            .build()
            .unwrap()
    } else {
        gst::ElementFactory::make("quinnwtsink")
            .name("moq-sink")
            .property("url", relay_url.to_string())
            .property("server-name", DEFAULT_SERVER_NAME)
            .property("secure-connection", false)
            .property_from_str("role", "client")
            .property("timeout", 1u32)
            .build()
            .unwrap()
    };

    setup_publisher_pipeline(&pipeline, &moqmux, &sink, &cli.uri, cli.no_audio);

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
            pipeline
                .debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "moq-publisher-quitting");

            pipeline.send_event(gst::event::Eos::new());
        }
    ))
    .unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    glib::timeout_add_seconds_once(
        5,
        glib::clone!(
            #[weak]
            pipeline,
            move || {
                pipeline.debug_to_dot_file_with_ts(gst::DebugGraphDetails::all(), "moq-publisher");
            }
        ),
    );

    main_loop.run();

    pipeline.set_state(gst::State::Null).unwrap();
}
