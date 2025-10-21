use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

use gst::prelude::*;
use gst_app::AppSinkCallbacks;
use sha2::Digest;
use tempfile::NamedTempFile;

static WASM_MODULE_BYTES: &[u8] = include_bytes!("testfilter.wasm");

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        gstwasm::plugin_register_static().expect("wasmvideofilter test");
    });
}

fn get_expected_checksum() -> &'static str {
    // Got from manually running the pipeline and hashing.
    "7931dae4725e3efe5918b22ba13c83f26af3ce14565a88860317814d758695cf"
}
#[test]
fn test_filter() {
    init();

    let mut wasm_file = NamedTempFile::new().unwrap();
    wasm_file.write_all(WASM_MODULE_BYTES).unwrap();
    let wasm_path = wasm_file.path().to_str().unwrap();

    let pipeline = gst::parse::launch(
        format!(
            "videotestsrc num-buffers=5 pattern=smpte ! videoconvert ! 
            video/x-raw,format=BGRA,width=1920,height=1080 ! 
            wasmvideofilter module={} config-str=sepia 
            ! appsink name=sink",
            wasm_path
        )
        .as_str(),
    )
    .unwrap();

    let sink = &pipeline
        .downcast_ref::<gst::Bin>()
        .expect("Pipeline is not a bin")
        .by_name("sink")
        .expect("Pipeline should have an element named sink")
        .dynamic_cast::<gst_app::AppSink>()
        .expect("sink should be an appsink");

    sink.set_property("emit-signals", true);
    sink.set_property("max-buffers", 1u32);
    sink.set_property("drop", false);

    let hasher = Arc::new(Mutex::new(sha2::Sha256::default()));

    let hasher_clone = hasher.clone();
    sink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().expect("appsink should have a sample");
                let buffer = sample.buffer().expect("Sample should have a buffer");
                let map = buffer.map_readable().expect("Buffer should be readable");
                let mut hasher = hasher_clone.lock().unwrap();
                hasher.update(map.as_slice());
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    let bus = pipeline.bus().unwrap();
    pipeline
        .set_state(gst::State::Playing)
        .expect("Pipeline should go to playing");

    let mut found_eos = false;
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(e) => {
                panic!(
                    "Pipeline error: {:?} - {}",
                    e.error(),
                    e.debug().unwrap_or_default()
                )
            }
            MessageView::Eos(_) => {
                found_eos = true;
                break;
            }
            _ => (),
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .expect("Pipeline should go to null");
    assert!(found_eos, "Pipeline did not post EOS");

    let final_hash = hasher.lock().unwrap().clone().finalize();
    let checksum = format!("{:x}", final_hash);
    println!("Got checksum {}", checksum);
    assert_eq!(checksum, get_expected_checksum());
}
