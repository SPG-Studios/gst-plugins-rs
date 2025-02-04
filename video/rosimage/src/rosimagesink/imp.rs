use std::sync::atomic::AtomicU64;

use gst::glib;
use gst::prelude::*;
use gst_base::subclass::prelude::*;

use once_cell::sync::Lazy;
use crate::sensor_msgs::Image as RosImageMsg;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rosimagesink",
        gst::DebugColorFlags::empty(),
        Some("ROS1 Image topic sink"),
    )
});

struct PublisherInfo {
    publisher : rosrust::Publisher<RosImageMsg>,
    topic : String
}

#[derive(Default)]
pub struct RosImageSink {
    publisher_info : std::sync::Arc<std::sync::Mutex<Option<PublisherInfo>>>
}

static ROS_IMAGE_SINK_ID : AtomicU64 = AtomicU64::new(0);

impl RosImageSink {
    fn ensure_init(&self) -> () {
        if !rosrust::is_initialized() {
            rosrust::init(&format!("gst_to_ros__{}", ROS_IMAGE_SINK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RosImageSink {
    const NAME: &'static str = "GstROSImageSink";
    type Type = super::RosImageSink;
    type ParentType = gst_base::BaseSink;
}

impl ObjectImpl for RosImageSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("topic")
                .nick("ROS Topic")
                .blurb("The Image topic on which to publish ROS messages (e.g. /video_stream/image_raw)")
                .mutable_ready()
                .build()
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "topic" => {
                let value = value.get::<Option<String>>().expect("type checked upstream");
                let mut publisher_info = self.publisher_info.lock().unwrap();
                match value {
                    None => {
                        // topic removed, delete publisher
                        let _ = publisher_info.take();
                    },
                    Some(topic) => {
                        if publisher_info.iter().any(|p| p.topic == topic) {
                            // topic set to same value, do nothing
                        } else {
                            let _ = publisher_info.take();
                            self.ensure_init();
                            *publisher_info = Some(PublisherInfo { publisher: rosrust::publish(&topic, 1).unwrap(), topic });
                        }
                    }
                }
                
            }
            _ => unimplemented!(),
        }
    }

    // fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
    //     match pspec.name() {
    //         "topic" => self
    //             .topic
    //             .get()
    //             .unwrap_or(&String::from("(uninitialized)"))
    //             .to_value(),
    //         _ => unimplemented!(),
    //     }
    // }
}

impl GstObjectImpl for RosImageSink {}

impl BaseSinkImpl for RosImageSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.ensure_init();
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        rosrust::shutdown();
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.ensure_init();
        let publisher_info = self.publisher_info.lock().unwrap();
        let publisher_info = match *publisher_info {
            Some(ref p) => p,
            None => {
                gst::warning!(CAT, "no topic set, cannot publish to ROS.");
                return Ok(gst::FlowSuccess::Ok)
            }
        };
        if publisher_info.publisher.subscriber_count() == 0 {
            gst::info!(
                CAT,
                "no subscribers on topic '{}', dropping frame",
                &publisher_info.topic
            );
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut msg = RosImageMsg::default();
        msg.data = vec![0; buffer.size()];
        msg.encoding = "rgb8".to_owned();

        let caps = self.obj().pads()[0].current_caps().unwrap();

        let width: i32 = caps.iter().next().unwrap().get("width").unwrap();
        let height: i32 = caps.iter().next().unwrap().get("height").unwrap();

        assert!(width > 0);
        assert!(height > 0);

        msg.width = width as u32;
        msg.height = height as u32;
        msg.step = 3 * (width as u32);

        buffer.copy_to_slice(0, &mut msg.data).unwrap();

        publisher_info.publisher.send(msg).unwrap();
        Ok(gst::FlowSuccess::Ok)
    }

    fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        self.parent_caps(filter)
    }
}

impl ElementImpl for RosImageSink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ROS Image topic source",
                "Source/Video",
                env!("CARGO_PKG_DESCRIPTION"),
                "Johannes Barthel <johannes.barthel@farming-revolution.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", "RGB")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
