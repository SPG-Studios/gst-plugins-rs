use std::sync::atomic::AtomicU64;

use gst::glib;
use gst::glib::subclass::SignalId;
use gst::prelude::*;
use gst::ClockTime;
use gst_base::subclass::base_src::CreateSuccess;
use gst_base::subclass::{base_src, prelude::*};
use gst_base::prelude::BaseSrcExt;
use gst_base::PushSrc;

use gst_video::VideoFormat;
use once_cell::sync::Lazy;

use crate::sensor_msgs::Image as RosImageMsg;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "rosimagesrc",
        gst::DebugColorFlags::empty(),
        Some("ROS1 Image topic source"),
    )
});

struct SubscriberInfo {
    _subscriber_raii : rosrust::Subscriber,
    topic : String,
}

#[derive(Default)]
pub struct RosImageSrc {
    channel: once_cell::sync::OnceCell<(crossbeam_channel::Sender<RosImageMsg>, crossbeam_channel::Receiver<RosImageMsg>)>,
//    create_called: std::sync::Arc<std::sync::atomic::AtomicU64>,
    subscriber_info: std::sync::Mutex<Option<SubscriberInfo>>,
    last_caps: std::sync::Mutex<Option<(VideoFormat, u32, u32)>>
}

fn encoding_to_gst(encoding: &str) -> Option<gst_video::VideoFormat> {
    match encoding {
        "rgb8" => Some(gst_video::VideoFormat::Rgb),
        "bgr8" => Some(gst_video::VideoFormat::Bgr),
        _ => None,
    }
}

static ROS_IMAGE_SRC_ID : AtomicU64 = AtomicU64::new(0);

impl RosImageSrc {
    fn ensure_init(&self, subscriber_info : Option<&mut Option<SubscriberInfo>>) -> () {
        let subscriber_info = match subscriber_info {
            None => &*self.subscriber_info.lock().unwrap(),
            Some(s) => s
        };
        if subscriber_info.is_some() {
            return;
        }
        self.channel.get_or_init(|| crossbeam_channel::unbounded());
        
        if !rosrust::is_initialized() {
            let pid = std::process::id();
            rosrust::init(&format!("ros_to_gst_pid_{}_src_{}", pid, ROS_IMAGE_SRC_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        }

        // self.subscriber.set(
        //     rosrust::subscribe(topic, 2, {
        //         let create_called = Arc::clone(&self.create_called);
        //         let topic = String::clone(&topic);
        //         move |v: RosImageMsg| {
        //             gst::info!(CAT, "Received image on topic '{}' in frame '{}'", topic, v.header.frame_id);
        //             if encoding_to_gst(&v.encoding).is_none() {
        //                 gst::error!(CAT, "Unknown ROS image format '{}' on topic '{}'. Not forwarding frame to GStreamer pipeline.", v.encoding, topic2);
        //             } else if let Err(e) = sender.try_send(v) {
        //                 let create_called = create_called.load(std::sync::atomic::Ordering::SeqCst);
        //                 gst::error!(CAT, "error sending an image frame to gstreamer: {:#?} | create_called: {}", e, create_called);
        //             }
        //         }
        //     }).map_err(|e| gst::error_msg!(gst::CoreError::Failed, ["{:?}", e]))?
        // ).map_err(|_e| gst::error_msg!(gst::CoreError::Failed, ["Tried to start RosImageSrc, but Subscriber was already there."]))?;

        // self.receiver
        //     .set(receiver)
        //     .expect("RosImageSrc.receiver initialized a second time. This is a bug.");
        
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RosImageSrc {
    const NAME: &'static str = "GstROSImageSrc";
    type Type = super::RosImageSrc;
    type ParentType = gst_base::PushSrc;
}

impl ObjectImpl for RosImageSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::builder("topic")
                .nick("ROS Topic")
                .blurb("The Image topic which to subscribe (e.g. /usb_cam/image_raw)")
                .mutable_ready()
                .build()
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "topic" => {
                let topic = value.get::<Option<String>>().expect("type checked upstream");
                let mut subscriber_info = self.subscriber_info.lock().unwrap();
                if let Some(ref subscriber_info) = &*subscriber_info {
                    if topic.iter().any(|topic| topic == &subscriber_info.topic) {
                        return;
                    }
                }

                let _ = subscriber_info.take(); // if there was one before, drop the subscriber, cancelling the subscription
                
                let topic = match topic {
                    Some(topic) => topic,
                    None => {
                        gst::info!(CAT, "topic was removed from rosimagesrc, no further data will be sent");
                        return
                    }
                };

                self.ensure_init(Some(&mut *subscriber_info));
                
                let sender = self.channel.get().unwrap().0.clone();
                *subscriber_info = Some(SubscriberInfo {
                    _subscriber_raii: rosrust::subscribe(&topic, 1, move |msg| {
                        let res = sender.try_send(msg);
                        if let Err(e) = res {
                            gst::error!(CAT, "error sending an image frame from subscriber to gstreamer: {:#?}", e);
                        }
                    }).unwrap(),
                    topic
                });
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

    // fn constructed(&self) {
    //     self.parent_constructed();
    //     self.obj().set_do_timestamp(true);
    //     self.obj().set_format(gst::Format::Time);
    //     assert!(self.is_seekable() == false);
    //     self.obj().set_live(true);
    // }
}

impl GstObjectImpl for RosImageSrc {}

impl BaseSrcImpl for RosImageSrc {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.ensure_init(None);
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

    // fn caps(&self, _filter: Option<&gst::Caps>) -> Option<gst::Caps> {
    //     self.ensure_subscriber().ok()?;
    //     let frame = self
    //         .receiver
    //         .get()
    //         .expect("receiver not initialized after call to ensure_subscriber, this is a bug.")
    //         .recv()
    //         .expect("error receiving frame while determining caps.");
    //     let encoding = encoding_to_gst(&frame.encoding)?;
    //     Some(
    //         gst_video::VideoInfo::builder(encoding, frame.width, frame.height)
    //             .fps(0)
    //             .build()
    //             .unwrap()
    //             .to_caps()
    //             .unwrap(),
    //     )
    // }
    fn negotiate(&self) -> Result<(), gst::LoggableError> {
        Ok(())
    }
}

impl PushSrcImpl for RosImageSrc {
    fn create(
        &self,
        buffer: Option<&mut gst::BufferRef>,
    ) -> Result<base_src::CreateSuccess, gst::FlowError> {
        // self.create_called
        //     .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        gst::info!(CAT, "RosImageSrc::create() called");
        self.ensure_init(None);

        let frame = self
            .channel
            .get()
            .expect("no receiver after call to ensure_subscriber, this is a bug")
            .1
            .recv()
            .expect("could not receive frame in RosImageSrc.create");

        let num_rows = frame.data.iter().filter(|x| **x==255).count() / (frame.width as usize);
        gst::info!(CAT, "RosImageSrc::create() got frame from channel with {num_rows} red rows");

        let encoding = encoding_to_gst(&frame.encoding).ok_or_else(|| {
            gst::error!(CAT, "incoming video frame has encoding '{}', which is not supported", &frame.encoding);
            gst::FlowError::NotSupported
        })?;
        
        // check whether we need to renegotiate caps (because the input format changed)
        { // Mutex scope for self.last_caps
            let mut last_caps = self.last_caps.lock().unwrap();
            let new_caps = Some((encoding, frame.width, frame.height));
            if new_caps != *last_caps {
                let caps = gst_video::VideoInfo::builder(encoding, frame.width, frame.height)
                 .fps(0)
                 .build()
                 .unwrap()
                 .to_caps()
                 .unwrap();

                self.set_caps(&caps).map_err(|e| {
                    gst::error!(CAT, "error renegotiating caps during create(): {:?}", e);
                    gst::FlowError::NotNegotiated
                })?;
                *last_caps = new_caps;
            }
        } // end of scope for MutexGuard of self.last_caps

        let time = self.obj().current_clock_time();
        dbg!(&time);

        if let Some(buf) = buffer {
            use std::io::Write;
            buf.set_size(frame.data.len());
            buf.set_pts(time);
            let mut buf = buf.as_cursor_writable().unwrap();
            buf.write(&frame.data).unwrap();
            return Ok(base_src::CreateSuccess::FilledBuffer);
        };

        let mut buf = gst::Buffer::from_slice(frame.data);
        //dbg!(buf.pts());
        //buf.get_mut().unwrap().set_pts(Some(time)); // this does not compile for some reason
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
