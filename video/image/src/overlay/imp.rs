// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on onvifmetadataoverlay, hsvdetectorm and gdkpixbufoverlay

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_video::VideoColorimetry;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use image::ImageReader;

use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use crate::cicp::ImageCicp;

pub(crate) static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "imagersoverlay",
        gst::DebugColorFlags::empty(),
        Some("image-rs overlay"),
    )
});

#[derive(Default)]
struct State {
    composition: Option<gst_video::VideoOverlayComposition>,
    location: String,
    image: Option<gst::Buffer>,
    update_composition: bool,
}

#[derive(Default)]
struct Settings {
    location: String,
    offset_x: i32,
    offset_y: i32,
    relative_x: f64,
    relative_y: f64,
    overlay_width: u32,
    overlay_height: u32,
    alpha: f32,
}

#[derive(Default)]
pub struct ImageRsOverlay {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl ImageRsOverlay {
    fn update_composition<'a>(&'a self, state: &mut MutexGuard<'a, State>) {
        let in_info = self.obj().input_video_info().unwrap();
        let video_width: i64 = in_info.width().into();
        let video_height: i64 = in_info.height().into();

        if state.composition.is_some() {
            state.composition = None;
        }

        let settings = self.settings.lock().unwrap();
        if settings.alpha == 0.0 || state.image.is_none() {
            return;
        }

        let overlay_pixels = state.image.as_ref().unwrap();
        let overlay_meta = overlay_pixels.meta::<gst_video::VideoMeta>().unwrap();
        let width: i64 = settings.overlay_width.max(overlay_meta.width()).into();
        let height: i64 = settings.overlay_height.max(overlay_meta.height()).into();

        let x = if settings.offset_x < 0 {
            video_width + settings.offset_x as i64 - width
                + (settings.relative_x * video_width as f64) as i64
        } else {
            settings.offset_x as i64 + (settings.relative_x * video_width as f64) as i64
        }
        .clamp(i32::MIN as i64, i32::MAX as i64);
        let y = if settings.offset_y < 0 {
            video_height + settings.offset_y as i64 - height
                + (settings.relative_y * video_height as f64) as i64
        } else {
            settings.offset_y as i64 + (settings.relative_y * video_height as f64) as i64
        }
        .clamp(i32::MIN as i64, i32::MAX as i64);

        gst::debug!(
            CAT,
            imp = self,
            "overlay image dimensions: {} x {}, alpha={}",
            overlay_meta.width(),
            overlay_meta.height(),
            settings.alpha
        );

        gst::debug!(
            CAT,
            imp = self,
            "properties: x,y: {},{} ({}%,{}%) - WxH: {}x{}",
            settings.offset_x,
            settings.offset_y,
            settings.relative_x * 100.0,
            settings.relative_y * 100.0,
            settings.overlay_width,
            settings.overlay_height
        );

        let mut rect = gst_video::VideoOverlayRectangle::new_raw(
            overlay_pixels,
            x as i32,
            y as i32,
            width as u32,
            height as u32,
            gst_video::VideoOverlayFormatFlags::empty(),
        );
        if settings.alpha != 1.0 {
            rect.get_mut().unwrap().set_global_alpha(settings.alpha);
        }

        drop(settings);

        state.composition = Some(gst_video::VideoOverlayComposition::new(Some(&rect)).unwrap());
        state.update_composition = false;
    }

    fn load_image<'a>(
        &'a self,
        state: &mut MutexGuard<'a, State>,
    ) -> Result<(), gst::ErrorMessage> {
        let location = {
            let settings = self.settings.lock().unwrap();
            if state.location == settings.location {
                return Ok(());
            }

            settings.location.clone()
        };

        // Using BufReader removes the code duplication that happens
        // when using image-rs's own reading facilities: 7.6 => 7.2MB
        let fs = std::fs::File::open(&location).map_err(|v| {
            gst::error_msg!(
                gst::ResourceError::OpenRead,
                ["Could not load overlay image: {}", v]
            )
        })?;
        let cursor = std::io::BufReader::new(fs);
        let reader = ImageReader::new(cursor);
        let mut argb_image = match reader.decode().map_err(|v| {
            gst::error_msg!(
                gst::StreamError::Decode,
                ["Could not decode overlay image container: {}", v]
            )
        })? {
            image::DynamicImage::ImageRgba8(v) => v,
            v => v.to_rgba8(),
        };
        // Correct for BGRA order
        // https://github.com/image-rs/image/commit/38456b67a943f39dfad7ab35589afe7a86ea4643
        // https://github.com/image-rs/image/pull/2712/changes#diff-e2d0a143bdfdd2f70d37b1c74f277e94b4ac51ea63824ea8fabbafbd7015ec9c
        {
            for pix in argb_image.as_chunks_mut::<4>().0 {
                pix.swap(0, 2);
            }
        }
        let format = {
            let width = argb_image.width();
            let height = argb_image.height();
            let cwh_stride = argb_image.as_flat_samples().strides_cwh();
            // RGBA is a single plane
            let strides: [i32; 4] = [cwh_stride.2.try_into().unwrap(), 0, 0, 0];
            // FIXME: should this be unwrapped?
            let color_info =
                match VideoColorimetry::try_from(ImageCicp::from(argb_image.color_space())) {
                    Ok(v) => Some(v),
                    Err(v) => {
                        gst::warning!(CAT, imp = self, "Failed converting to VideoInfo: {v}");
                        None
                    }
                };
            gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgra, width, height)
                .stride(&strides)
                .colorimetry_if_some(color_info.as_ref())
                .build()
                .unwrap()
        };
        let mut buffer = gst::Buffer::from_slice(argb_image.into_vec());

        gst_video::VideoMeta::add_full(
            buffer.get_mut().unwrap(),
            gst_video::VideoFrameFlags::empty(),
            format.format(),
            format.width(),
            format.height(),
            format.offset(),
            format.stride(),
        )
        .unwrap();

        state.location = location;
        state.image = Some(buffer);
        state.update_composition = true;

        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ImageRsOverlay {
    const NAME: &'static str = "GstImageRsOverlay";
    type Type = super::Overlay;
    type ParentType = gst_video::VideoFilter;
}

impl ObjectImpl for ImageRsOverlay {
    fn constructed(&self) {
        self.parent_constructed();

        image_extras::register();
    }
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("location")
                    .blurb("Location of image file to overlay")
                    .default_value(None)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt::builder("offset-x")
                    .nick("X Offset")
                    .blurb("For positive value, horizontal offset of overlay image in pixels from left of video image. For negative value, horizontal offset of overlay image in pixels from right of video image")
                    .default_value(0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecInt::builder("offset-y")
                    .nick("Y Offset")
                    .blurb("For positive value, vertical offset of overlay image in pixels from top of video image. For negative value, vertical offset of overlay image in pixels from bottom of video image")
                    .default_value(0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("relative-x")
                    .nick("Relative X Offset")
                    .blurb("Horizontal offset of overlay image in fractions of video image width, from top-left corner of video image (in relative positioning)")
                    .minimum(-1.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecDouble::builder("relative-y")
                    .nick("Relative Y Offset")
                    .blurb("Vertical offset of overlay image in fractions of video image width, from top-left corner of video image (in relative positioning)")
                    .minimum(-1.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("overlay-width")
                    .nick("Overlay Width")
                    .blurb("Width of overlay image in pixels (0 = same as overlay image)")
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecUInt::builder("overlay-height")
                    .nick("Overlay Height")
                    .blurb("Height of overlay image in pixels (0 = same as overlay image")
                    .controllable()
                    .mutable_playing()
                    .build(),
                glib::ParamSpecFloat::builder("alpha")
                    .nick("Alpha")
                    .blurb("Global alpha of overlay image")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(1.0)
                    .controllable()
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => settings.location.to_value(),
            "offset-x" => settings.offset_x.to_value(),
            "offset-y" => settings.offset_y.to_value(),
            "relative-x" => settings.relative_x.to_value(),
            "relative-y" => settings.relative_y.to_value(),
            "overlay-width" => settings.overlay_width.to_value(),
            "overlay-height" => settings.overlay_height.to_value(),
            "alpha" => settings.alpha.to_value(),
            _ => unimplemented!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut state = self.state.lock().unwrap();
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "location" => {
                let value = value.get().expect("type checked upstream");
                settings.location = value;
                state.update_composition = true;
            }
            "offset-x" => {
                let value = value.get().expect("type checked upstream");
                settings.offset_x = value;
                state.update_composition = true;
            }
            "offset-y" => {
                let value = value.get().expect("type checked upstream");
                settings.offset_y = value;
                state.update_composition = true;
            }
            "relative-x" => {
                let value = value.get().expect("type checked upstream");
                settings.relative_x = value;
                state.update_composition = true;
            }
            "relative-y" => {
                let value = value.get().expect("type checked upstream");
                settings.relative_y = value;
                state.update_composition = true;
            }
            "overlay-width" => {
                let value = value.get().expect("type checked upstream");
                settings.overlay_width = value;
                state.update_composition = true;
            }
            "overlay-height" => {
                let value = value.get().expect("type checked upstream");
                settings.overlay_height = value;
                state.update_composition = true;
            }
            "alpha" => {
                let value = value.get().expect("type checked upstream");
                settings.alpha = value;
                state.update_composition = true;
            }
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for ImageRsOverlay {}

impl ElementImpl for ImageRsOverlay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs overlay",
                "Video/Overlay",
                "Renders images decoded with image-rs over raw video frames",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new().build();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, imp = self, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused | gst::StateChange::PausedToReady => {
                // Reset the whole state
                let mut state = self.state.lock().unwrap();
                *state = State::default();
            }
            _ => (),
        }

        self.parent_change_state(transition)
    }
}

impl BaseTransformImpl for ImageRsOverlay {
    const MODE: gst_base::subclass::BaseTransformMode = gst_base::subclass::BaseTransformMode::Both;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let is_location_empty = self.settings.lock().unwrap().location.is_empty();

        if !is_location_empty {
            self.obj().set_passthrough(false);
        } else {
            gst::warning!(CAT, imp = self, "no image location set, doing nothing");
            self.obj().set_passthrough(true);
        }
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        state.composition = None;
        state.image = None;
        Ok(())
    }

    fn before_transform(&self, inbuf: &gst::BufferRef) {
        let timestamp = inbuf.pts();
        let stream_time = self
            .obj()
            .segment()
            .downcast::<gst::ClockTime>()
            .ok()
            .and_then(|v| v.to_stream_time(timestamp));
        if let Some(stream_time) = stream_time
            && let Err(e) = self.obj().sync_values(stream_time)
        {
            gst::log!(CAT, imp = self, "{e}");
        }

        let mut set_passthrough = false;
        let has_no_composition = {
            let mut state = self.state.lock().unwrap();
            if let Err(err) = self.load_image(&mut state) {
                self.post_error_message(err);
                return;
            }
            if state.update_composition {
                self.update_composition(&mut state);
                set_passthrough = true;
            }
            state.composition.is_none()
        };
        if set_passthrough {
            self.obj().set_passthrough(has_no_composition);
        }
    }
}

impl VideoFilterImpl for ImageRsOverlay {
    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let state = self.state.lock().unwrap();
        if let Some(v) = &state.composition {
            // FIXME: negotiate and do metadata attaching when possible
            // See cea608overlay and
            // https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/merge_requests/2856#note_3330298
            v.blend(frame).map_err(|v| {
                gst::error!(CAT, imp = self, "Blending failed: {}", v);
                gst::FlowError::Error
            })?
        }
        Ok(gst::FlowSuccess::Ok)
    }
}
