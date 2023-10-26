use gst::prelude::*;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstqoa::plugin_register_static().expect("Failed to register csound plugin");
    });
}

#[test]
fn find_the_type_of_qoa_data() {
    init();

    let src = gst_app::AppSrc::builder()
        .is_live(true)
        .build();

    let typefind = gst::ElementFactory::make("typefind").build().unwrap();
    let fakesink = gst::ElementFactory::make("fakesink").build().unwrap();

    let pipeline = gst::Pipeline::new();
    pipeline.add_many(&[src.upcast_ref(), &typefind, &fakesink]).unwrap();
    gst::Element::link_many(&[src.upcast_ref(), &typefind, &fakesink]).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

    // Create some fake QOA data
    let mut qoa_data = Vec::new();
    qoa_data.extend(qoaudio::QOA_MAGIC.to_be_bytes());
    qoa_data.extend(&[0u8; 20]);
    src.push_buffer(gst::Buffer::from_slice(qoa_data)).unwrap();

    src.end_of_stream().unwrap();

    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                eprintln!(
                    "Error received from element {:?}: {}",
                    err.src().map(|s| s.path_string()),
                    err.error()
                );
                eprintln!("Debugging information: {:?}", err.debug());
                pipeline.set_state(gst::State::Null).unwrap();
                unreachable!();
            }
            MessageView::Eos(..) => break,
            _ => (),
        }
    }

    let pad = typefind.static_pad("src").unwrap();
    let caps = pad.current_caps().unwrap();
    assert_eq!(caps.to_string(), "audio/x-qoa");
}

// #[test]
// fn when_data_is_not_qoa() {
//     init();
//
// }
