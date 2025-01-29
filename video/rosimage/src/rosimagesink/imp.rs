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

struct Dimensions {
    width: i32,
    height: i32,
}

#[derive(Default)]
pub struct RosImageSink {
    publisher: once_cell::sync::OnceCell<rosrust::Publisher<RosImageMsg>>,
    topic: once_cell::sync::OnceCell<String>,
    dimensions: once_cell::sync::OnceCell<Dimensions>,
}

impl RosImageSink {
    fn ensure_publisher(&self) -> Result<(), gst::ErrorMessage> {
        if self.publisher.get().is_some() {
            return Ok(());
        }

        let topic = self.topic.get().ok_or_else(|| {
            gst::error_msg!(
                gst::CoreError::Negotiation,
                ["ROS topic has not been specified"]
            )
        })?;

        rosrust::init(&format!("gst_to_ros__{}", topic.replace('/', "_")));
        self.publisher
            .set(
                rosrust::publish(topic, 1)
                    .map_err(|e| gst::error_msg!(gst::CoreError::Failed, ["{:?}", e]))?,
            )
            .map_err(|_e| ())
            .expect("tried to set publisher when it already existed, this is a bug");

        return Ok(());
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RosImageSink {
    const NAME: &'static str = "rosimagesink";
    type Type = super::RosImageSink;
    type ParentType = gst_base::BaseSink;
}

impl ObjectImpl for RosImageSink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![{
                let mut b = glib::ParamSpecString::builder("topic");
                b.set_nick(Some("ROS Topic"));
                b.set_blurb(Some("The Image topic on which to publish ROS messages (e.g. /video_stream/image_raw)"));
                b.set_flags(glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY);
                b.build()
            }]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "topic" => {
                let value: String = value.get().expect("type checked upstream");
                self.topic
                    .set(value)
                    .expect("could not save topic to self.topic");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "topic" => self
                .topic
                .get()
                .unwrap_or(&String::from("(uninitialized)"))
                .to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RosImageSink {}

impl BaseSinkImpl for RosImageSink {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.ensure_publisher()?;
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        rosrust::shutdown();
        Ok(())
    }

    fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::info!(
            CAT,
            "Received image for publishing on topic '{}'",
            self.topic.get().map(String::as_str).unwrap_or("(unknown)")
        );

        self.ensure_publisher().unwrap();
        let publisher = self.publisher.get().unwrap();
        if publisher.subscriber_count() == 0 {
            gst::info!(
                CAT,
                "no subscribers on topic '{}', dropping frame",
                self.topic.get().map(String::as_str).unwrap_or("(unknown)")
            );
            return Ok(gst::FlowSuccess::Ok);
        }

        let mut msg = RosImageMsg::default();
        msg.data = vec![0; buffer.size()];
        msg.encoding = "rgb8".to_owned();

        let dims = self.dimensions.get_or_init(|| {
            let caps = self.obj().pads()[0].current_caps().unwrap();

            gst::info!(
                CAT,
                "Initializing caps for topic '{}': {:?}",
                self.topic.get().map(String::as_str).unwrap_or("(unknown)"),
                &caps
            );

            let width: i32 = caps.iter().next().unwrap().get("width").unwrap();
            let height: i32 = caps.iter().next().unwrap().get("height").unwrap();

            assert!(width > 0);
            assert!(height > 0);

            Dimensions { width, height }
        });

        msg.width = dims.width as u32;
        msg.height = dims.height as u32;
        msg.step = 3 * (dims.width as u32);

        buffer.copy_to_slice(0, &mut msg.data).unwrap();

        publisher.send(msg).unwrap();
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
