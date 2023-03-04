use gst::glib;
use gst::prelude::*;
use gst::ClockTime;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::{base_src, prelude::*};
use gst_base::traits::BaseSrcExt;
use gst_base::PushSrc;

use once_cell::sync::Lazy;

use rosrust_msg::sensor_msgs::Image;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rosimagesrc",
        gst::DebugColorFlags::empty(),
        Some("ROS1 Image topic source"),
    )
});

#[derive(Default)]
pub struct RosImageSrc {
    receiver: once_cell::sync::OnceCell<crossbeam_channel::Receiver<Image>>,
    subscriber: once_cell::sync::OnceCell<rosrust::Subscriber>,
    topic: once_cell::sync::OnceCell<String>,
    create_called: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

fn encoding_to_gst(encoding: &str) -> Option<gst_video::VideoFormat> {
    match encoding {
        "rgb8" => Some(gst_video::VideoFormat::Rgb),
        "bgr8" => Some(gst_video::VideoFormat::Bgr),
        _ => None,
    }
}

impl RosImageSrc {
    fn ensure_subscriber(&self) -> Result<(), gst::ErrorMessage> {
        if self.subscriber.get().is_some() {
            return Ok(());
        }
        let (sender, receiver) = crossbeam_channel::bounded(1);
        let topic = self.topic.get().ok_or_else(|| {
            gst::error_msg!(
                gst::CoreError::Negotiation,
                ["ROS topic has not been specified"]
            )
        })?;
        let topic2 = topic.clone();
        rosrust::init(&format!("ros_to_gst__{}", topic.replace('/', "_")));

        let create_called1 = self.create_called.clone();

        self.subscriber.set(
            rosrust::subscribe(topic, 2, move |v: rosrust_msg::sensor_msgs::Image| {
                gst::info!(CAT, "Received image on topic '{}' in frame '{}'", topic2, v.header.frame_id);
                if encoding_to_gst(&v.encoding).is_none() {
                    gst::error!(CAT, "Unknown ROS image format '{}' on topic '{}'. Not forwarding frame to GStreamer pipeline.", v.encoding, topic2);
                } else if let Err(e) = sender.try_send(v) {
                    let create_called = create_called1.load(std::sync::atomic::Ordering::SeqCst);
                    gst::error!(CAT, "error sending an image frame to gstreamer: {:#?} | create_called: {}", e, create_called);
                }
            }).map_err(|e| gst::error_msg!(gst::CoreError::Failed, ["{:?}", e]))?
        ).map_err(|_e| gst::error_msg!(gst::CoreError::Failed, ["Tried to start RosImageSrc, but Subscriber was already there."]))?;

        self.receiver
            .set(receiver)
            .expect("RosImageSrc.receiver initialized a second time. This is a bug.");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RosImageSrc {
    const NAME: &'static str = "rosimagesrc";
    type Type = super::RosImageSrc;
    type ParentType = gst_base::PushSrc;
}

impl ObjectImpl for RosImageSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![{
                let mut b = glib::ParamSpecString::builder("topic");
                b.set_nick(Some("ROS Topic"));
                b.set_blurb(Some(
                    "The Image topic which to subscribe (e.g. /usb_cam/image_raw)",
                ));
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

    fn constructed(&self) {
        self.parent_constructed();
        self.obj().set_do_timestamp(true);
        self.obj().set_format(gst::Format::Time);
        assert!(self.is_seekable() == false);
        self.obj().set_live(true);
    }
}

impl GstObjectImpl for RosImageSrc {}

impl BaseSrcImpl for RosImageSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.ensure_subscriber()?;
        for pad in self.obj().pads().iter_mut() {
            pad.activate_mode(gst::PadMode::Push, true).unwrap();
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        rosrust::shutdown();
        Ok(())
    }

    fn is_seekable(&self) -> bool {
        false
    }

    fn caps(&self, _filter: Option<&gst::Caps>) -> Option<gst::Caps> {
        self.ensure_subscriber().ok()?;
        let frame = self
            .receiver
            .get()
            .expect("receiver not initialized after call to ensure_subscriber, this is a bug.")
            .recv()
            .expect("error receiving frame while determining caps.");
        let encoding = encoding_to_gst(&frame.encoding)?;
        Some(
            gst_video::VideoInfo::builder(encoding, frame.width, frame.height)
                .fps(0)
                .build()
                .unwrap()
                .to_caps()
                .unwrap(),
        )
    }
}

impl PushSrcImpl for RosImageSrc {
    fn create(
        &self,
        buffer: Option<&mut gst::BufferRef>,
    ) -> Result<base_src::CreateSuccess, gst::FlowError> {
        self.create_called
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        gst::info!(CAT, "RosImageSrc::create() called");
        self.ensure_subscriber()
            .expect("create called when subscriber was not ready");

        let obj = self.obj();
        let t = obj.current_clock_time();

        self.obj().suggest_next_sync();

        let frame = self
            .receiver
            .get()
            .expect("no receiver after call to ensure_subscriber, this is a bug")
            .recv()
            .expect("could not receive frame in RosImageSrc.create");

        if let Some(buf) = buffer {
            use std::io::Write;
            buf.set_size(frame.data.len());
            let mut buf = buf.as_cursor_writable().unwrap();
            buf.write(&frame.data).unwrap();
            return Ok(base_src::CreateSuccess::FilledBuffer);
        };

        let mut buf = gst::Buffer::from_slice(frame.data);
        Ok(base_src::CreateSuccess::NewBuffer(buf))
    }
}

impl ElementImpl for RosImageSrc {
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
            let caps = gst::Caps::builder("video/x-raw").build();
            let mut src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();
            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
