// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on gstpixbufdec

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_video::VideoColorimetry;

use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};

use std::collections::VecDeque;
use std::io::{BufRead, Cursor, Seek};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::buffer::GStreamerImage;
use crate::cicp::ImageCicp;
use crate::format::Format;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "imagersdec",
        gst::DebugColorFlags::empty(),
        Some("image-rs decoder for still image formats"),
    )
});

#[derive(Default)]
struct Settings {
    max_size: u64,
    max_alloc: u64,
}

#[derive(Default)]
struct State {
    buffers: Vec<gst::Buffer>,
    format_from_caps: Option<Format>,
    total_size: usize,
    duration: Option<gst::ClockTime>,
    in_fps: Option<gst::Fraction>,
    in_par: Option<gst::Fraction>,
    info: Option<gst_video::VideoInfo>,
    pending_events: VecDeque<gst::Event>,
    packetized: bool,
}

trait ImageRsBuffer<'a>: BufRead + Seek {}

impl<'a, T: BufRead + Seek> ImageRsBuffer<'a> for T {}

pub struct ImageRsDecoder {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl ImageRsDecoder {
    fn dec_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {buffer:?}");

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let timestamp = buffer.pts();

        if state.packetized
            || settings.max_size == 0
            || (state.total_size + buffer.size()) as u64 <= settings.max_size
        {
            gst::log!(CAT, imp = self, "Writing buffer size {}", buffer.size());
            state.total_size += buffer.size();
            state.buffers.push(buffer);

            if state.packetized {
                return self.decode(timestamp, settings, state);
            }

            Ok(gst::FlowSuccess::Ok)
        } else {
            gst::error!(
                CAT,
                obj = pad,
                "Exhausted memory limit of {:?} bytes",
                settings.max_size
            );
            Err(gst::FlowError::Error)
        }
    }

    fn convert_format(&self, image: DynamicImage) -> (DynamicImage, gst_video::VideoFormat) {
        use DynamicImage::*;
        use gst_video::VideoFormat;
        match image {
            ImageRgb8(_) => (image, VideoFormat::Rgb),
            ImageRgba8(_) => (image, VideoFormat::Rgba),
            ImageLuma8(_) => (image, VideoFormat::Gray8),
            #[cfg(target_endian = "little")]
            ImageLuma16(_) => (image, VideoFormat::Gray16Le),
            #[cfg(target_endian = "big")]
            ImageLuma16(_) => (image, VideoFormat::Gray16Be),
            #[cfg(target_endian = "little")]
            ImageRgba16(_) => (image, VideoFormat::Rgba64Le),
            #[cfg(target_endian = "big")]
            ImageRgba16(_) => (image, VideoFormat::Rgba64Be),
            v => {
                gst::trace!(
                    CAT,
                    imp = self,
                    "Format {:?} not supported, converting to RGB(A)",
                    v.color()
                );
                if v.has_alpha() {
                    (v.to_rgba8().into(), VideoFormat::Rgba)
                } else {
                    (v.to_rgb8().into(), VideoFormat::Rgb)
                }
            }
        }
    }

    fn set_format_from_caps(&self, event_caps: &gst::event::Caps) -> Result<(), gst::ErrorMessage> {
        let s = event_caps.caps().structure(0).unwrap();
        let mut state = self.state.lock().unwrap();
        state.format_from_caps = Some(s.name().as_str().try_into()?);
        state.in_fps = s.get::<gst::Fraction>("framerate").ok();
        state.in_par = s.get::<gst::Fraction>("pixel-aspect-ratio").ok();
        state.duration = s.get::<gst::ClockTime>("duration").ok();

        gst::info!(
            CAT,
            imp = self,
            "format {:?} fps {:?} pixel-aspect-ratio {:?} duration {:?}",
            state.format_from_caps,
            state.in_fps,
            state.in_par,
            state.duration
        );

        Ok(())
    }

    fn metadata_from_decoder(&self, decoder: &mut impl ImageDecoder) -> gst::TagList {
        let exif = match decoder.exif_metadata() {
            Ok(v) => v,
            Err(v) => {
                gst::warning!(CAT, imp = self, "Failed retrieving EXIF metadata: {v}");
                None
            }
        };

        let xmp = match decoder.xmp_metadata() {
            Ok(v) => v,
            Err(v) => {
                gst::warning!(CAT, imp = self, "Failed retrieving XMP metadata: {v}");
                None
            }
        };

        let icc = match decoder.icc_profile() {
            Ok(v) => v,
            Err(v) => {
                gst::warning!(CAT, imp = self, "Failed retrieving ICC profile: {v}");
                None
            }
        };

        let iptc = match decoder.iptc_metadata() {
            Ok(v) => v,
            Err(v) => {
                gst::warning!(CAT, imp = self, "Failed retrieving IPTC metadata: {v}");
                None
            }
        };

        let tags = gst::TagList::new();

        if let Some(v) = exif {
            let buf = gst::Buffer::from_mut_slice(v);
            let v_rust = unsafe {
                let v = gst_tag::ffi::gst_tag_list_from_exif_buffer(
                    buf.as_mut_ptr(),
                    #[cfg(target_endian = "little")]
                    gst::glib::ffi::G_LITTLE_ENDIAN,
                    #[cfg(target_endian = "big")]
                    gst::glib::ffi::G_BIG_ENDIAN,
                    0,
                );

                gst::TagList::from_glib_full(v)
            };
            tags.merge(&v_rust, gst::TagMergeMode::Append);
        };

        if let Some(v) = xmp {
            let buf = gst::Buffer::from_mut_slice(v);
            let v_rust = unsafe {
                let v = gst_tag::ffi::gst_tag_list_from_xmp_buffer(buf.as_mut_ptr());

                gst::TagList::from_glib_full(v)
            };
            tags.merge(&v_rust, gst::TagMergeMode::Append);
        };

        // These go into a separate structure
        let mut metadata_blobs = gst::TagList::new();

        if let Some(v) = iptc {
            let buf = gst::Buffer::from_mut_slice(v);
            let caps = gst::Caps::new_empty_simple("application/rdf+xml");
            let info = gst::Structure::new_empty("application/rdf+xml");

            let tagsample = gst::Sample::builder()
                .buffer(&buf)
                .caps(&caps)
                .info(info)
                .build();

            if let Some(v) = metadata_blobs.get_mut() {
                v.add::<gst::tags::Attachment>(&tagsample, gst::TagMergeMode::Append);
            }
        };

        if let Some(v) = icc {
            let buf = gst::Buffer::from_mut_slice(v);
            let caps = gst::Caps::new_empty_simple("application/vnd.iccprofile");
            let mut info = gst::Structure::new_empty("application/vnd.iccprofile");
            // FIXME: image-rs's png reader does not expose the profile name
            // see impl StreamingDecoder::parse_iccp_raw in the PNG crate
            info.set("icc-name", "(embedded profile from image-rs)");
            let tagsample = gst::Sample::builder()
                .buffer(&buf)
                .caps(&caps)
                .info(info)
                .build();

            if let Some(v) = metadata_blobs.get_mut() {
                v.add::<gst::tags::Attachment>(&tagsample, gst::TagMergeMode::Append);
            }
        }

        if metadata_blobs.n_tags() > 0 {
            tags.merge(&metadata_blobs, gst::TagMergeMode::Append);
        }

        tags
    }

    #[inline]
    fn render_single_frame<'a>(
        &'a self,
        settings: MutexGuard<'a, Settings>,
        mut state: MutexGuard<'a, State>,
        source: &mut dyn ImageRsBuffer<'a>,
        timestamp: Option<gst::ClockTime>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut reader = ImageReader::new(source);

        let format = state
            .format_from_caps
            .ok_or(gst::FlowError::NotNegotiated)?;

        if let Ok(v) = ImageFormat::try_from(format) {
            reader.set_format(v);
        }

        if settings.max_alloc != 0 {
            let mut limits = Limits::default();
            limits.max_alloc = Some(settings.max_alloc);
            reader.limits(limits);
        }

        drop(settings);

        let mut decoder = reader.into_decoder().map_err(|v| {
            gst::error!(CAT, imp = self, "Failed decoding single image: {v}");
            gst::FlowError::Error
        })?;

        let metadata = self.metadata_from_decoder(&mut decoder);

        let (image, fmt) =
            self.convert_format(DynamicImage::from_decoder(decoder).map_err(|v| {
                gst::error!(CAT, imp = self, "Failed decoding single image: {v}");
                gst::FlowError::Error
            })?);

        let wh = image.dimensions();

        let new_info = {
            let fps = state.in_fps;
            let par = state.in_par;

            let color_info = if image.color().has_color() {
                VideoColorimetry::try_from(ImageCicp(image.color_space()))
                    .inspect_err(|v| {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Failed converting to VideoColorimetry: {v}"
                        );
                    })
                    .ok()
            } else {
                None
            };

            gst_video::VideoInfo::builder(fmt, wh.0, wh.1)
                .fps_if_some(fps)
                .par_if_some(par)
                .colorimetry_if_some(color_info.as_ref())
                .build()
                .map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        [
                            "Format {fmt} with {}x{} @ {fps:?} not supported: {v}",
                            wh.0,
                            wh.1,
                        ]
                    );
                    gst::FlowError::NotNegotiated
                })
        }?;

        let packetized = state.packetized;
        let duration = state.duration;

        let pending_events = if state.info.as_ref().is_none_or(|v| !v.eq(&new_info)) {
            let caps = new_info.to_caps().map_err(|_| {
                gst::element_imp_error!(
                    self,
                    gst::ResourceError::Settings,
                    ["Invalid video info received: {:?}", new_info]
                );
                gst::FlowError::NotNegotiated
            })?;

            gst::debug!(
                CAT,
                imp = self,
                "Set size to {}x{}",
                new_info.width(),
                new_info.height()
            );

            state.info = Some(new_info);

            let pending_events = state.pending_events.drain(..).collect::<Vec<_>>();

            drop(state);

            let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));

            pending_events
        } else {
            drop(state);
            vec![]
        };

        for l in pending_events {
            self.srcpad.push_event(l);
        }

        let wrapper = image.wrap_for_gstreamer();

        let mut outbuf = gst::Buffer::from_slice(wrapper);
        {
            let outbuf = outbuf.get_mut().unwrap();
            outbuf.set_pts(timestamp);
            if packetized {
                outbuf.set_duration(duration);
            }
        }

        gst::debug!(CAT, imp = self, "pushing... {} bytes", outbuf.size());

        if metadata.n_tags() > 0 {
            let v = gst::event::Tag::new(metadata);
            self.srcpad.push_event(v);
        }

        self.srcpad.push(outbuf)
    }

    fn decode<'a>(
        &'a self,
        timestamp: Option<gst::ClockTime>,
        settings: MutexGuard<'a, Settings>,
        mut state: MutexGuard<'a, State>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if state.buffers.is_empty() {
            gst::error!(CAT, imp = self, "No buffers found");
            return Err(gst::FlowError::Error);
        }

        if state.packetized {
            assert_eq!(state.buffers.len(), 1);

            let buffer = state.buffers.drain(..).nth(0).unwrap();

            let mut cursor = Cursor::new(buffer.map_readable().unwrap());

            self.render_single_frame(settings, state, &mut cursor, timestamp)
        } else {
            let mut buf = Vec::with_capacity(state.total_size);

            for buffer in state.buffers.drain(..) {
                buf.extend_from_slice(&buffer.map_readable().expect("Failed to map buffer"));
            }

            let mut cursor = Cursor::new(buf);

            self.render_single_frame(settings, state, &mut cursor, timestamp)
        }
    }

    /// Takes caps and copies its video fields to tmpl_caps
    fn proxy_caps(&self, templ_caps: &gst::CapsRef, caps: &gst::CapsRef) -> gst::Caps {
        let mut result = gst::Caps::new_empty();

        for i in templ_caps.iter_with_features() {
            let name = i.0.name_id();
            let features = i.1;

            for _caps_s in caps.iter() {
                let mut tmp = gst::Caps::new_empty();

                let s = gst::Structure::new_empty_from_id(name);

                // FIXME: only pixel-aspect-ratio would be supported here,
                // but how would it interact with the setting value?
                // for v in [
                //     "width",
                //     "height",
                //     "framerate",
                //     "pixel-aspect-ratio",
                //     "colorimetry",
                //     "chroma-site"
                // ] {
                //     if let Ok(value) = caps_s.value(v) {
                //         s.set(v, value.clone());
                //     }
                // }

                tmp.get_mut()
                    .unwrap()
                    .append_structure_full(s, Some(features.to_owned()));

                result.merge(tmp);
            }
        }

        result
    }

    /// __gst_video_element_proxy_getcaps:
    ///
    /// @element: a #GstElement
    ///
    /// @sinkpad: the element's sink #GstPad
    ///
    /// @srcpad: the element's source #GstPad
    ///
    /// @initial_caps: initial caps
    ///
    /// @filter: filter caps
    ///
    /// Returns caps that express @initial_caps (or sink template caps if
    /// @initial_caps == NULL) restricted to resolution/format/...
    /// combinations supported by downstream elements (e.g. muxers).
    ///
    /// Returns: a #GstCaps owned by caller
    fn proxy_get_caps(
        &self,
        initial_caps: Option<&gst::CapsRef>,
        filter: Option<&gst::CapsRef>,
    ) -> gst::Caps {
        /* Allow downstream to specify width/height/framerate/PAR constraints
         * and forward them upstream for video converters to handle
         */
        let templ_caps = initial_caps
            .map(|v| v.copy())
            .unwrap_or_else(|| self.sinkpad.pad_template_caps());
        let allowed = {
            let src_templ_caps = self.srcpad.pad_template_caps();
            let peer_caps = if let Some(filter) = filter
                && !filter.is_any()
            {
                let proxy_filter = self.proxy_caps(&src_templ_caps, filter);
                self.srcpad.peer_query_caps(Some(&proxy_filter))
            } else {
                self.srcpad.peer_query_caps(None)
            };
            peer_caps.intersect_with_mode(&src_templ_caps, gst::CapsIntersectMode::First)
        };

        let fcaps = if allowed.is_any() {
            templ_caps
        } else if allowed.is_empty() {
            allowed
        } else {
            gst::log!(CAT, imp = self, "template caps {}", templ_caps);
            gst::log!(CAT, imp = self, "allowed caps {}", allowed);

            let mut fcaps = self
                .proxy_caps(&templ_caps, &allowed)
                .intersect(&templ_caps);

            drop(templ_caps);

            if let Some(f) = filter {
                gst::log!(CAT, imp = self, "intersecting with {}", f);
                fcaps = fcaps.intersect_with_mode(f, gst::CapsIntersectMode::First);
            }

            fcaps
        };

        gst::log!(CAT, imp = self, "proxy caps {}", fcaps);

        fcaps
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                let filter = q.filter();
                // See gst_video_decoder_sink_getcaps
                let caps = self.proxy_get_caps(None, filter);
                q.set_result(&caps);
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;
        gst::log!(CAT, obj = pad, "Handling event {:?}", event);

        let mut event_replace: Option<gst::Event> = None;
        let mut ret = true;
        let mut forward = true;

        match event.view() {
            EventView::Caps(v) => {
                if let Err(err) = self.set_format_from_caps(v) {
                    self.post_error_message(err);
                    ret = false;
                }
                forward = false;
            }
            EventView::Eos(..) | EventView::SegmentDone(..) => {
                let state = self.state.lock().unwrap();
                if !state.buffers.is_empty() {
                    let settings = self.settings.lock().unwrap();
                    if let Err(v) = self.decode(None, settings, state)
                        && v != gst::FlowError::Flushing
                        && v != gst::FlowError::NotLinked
                    {
                        if let EventView::SegmentDone(..) = event.view() {
                            forward = false
                        }
                        ret = false;
                    }
                }
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                state.pending_events.clear();
            }
            EventView::Segment(v) => {
                let mut state = self.state.lock().unwrap();
                let segment = v.segment();
                state.packetized = segment.format() == gst::Format::Time;
                if segment.format() != gst::Format::Time {
                    let seqnum = event.seqnum();
                    let output_segment = gst::FormattedSegment::<gst::ClockTime>::new();
                    event_replace = Some(
                        gst::event::Segment::builder(&output_segment)
                            .seqnum(seqnum)
                            .build(),
                    )
                }
            }
            _ => {}
        };

        if forward {
            if !self.srcpad.has_current_caps()
                && event.is_serialized()
                && event.type_() > gst::EventType::Caps
                && event.type_() != gst::EventType::FlushStop
                && event.type_() != gst::EventType::Eos
                && event.type_() != gst::EventType::SegmentDone
            {
                ret = true;
                let mut state = self.state.lock().unwrap();
                match event_replace {
                    Some(v) => state.pending_events.push_front(v),
                    None => state.pending_events.push_front(event),
                };
            } else {
                ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
            }
        }

        ret
    }

    fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::log!(CAT, obj = pad, "Handling event {event:?}");
        match event.view() {
            EventView::Seek(..) => false,
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                state.pending_events.clear();
                true
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ImageRsDecoder {
    const NAME: &'static str = "GstImageRsDecoder";
    type Type = super::Decoder;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |dec| dec.dec_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.sink_event(pad, event),
                )
            })
            .query_function(|pad, parent, query| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.sink_query(pad, query),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .event_function(|pad, parent, event| {
                ImageRsDecoder::catch_panic_pad_function(
                    parent,
                    || false,
                    |dec| dec.src_event(pad, event),
                )
            })
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for ImageRsDecoder {
    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("max-size-bytes")
                    .nick("Max. size (kB)")
                    .blurb("Max. amount of data to buffer (bytes, 0=disable)")
                    .default_value(10 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-alloc-bytes")
                    .nick("Memory allocation limits")
                    .blurb("Max. amount of data to allocate for decoding (bytes, 0=disable)")
                    .default_value(128 * 1024 * 1024)
                    .mutable_ready()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "max-alloc-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_alloc = value.get::<u64>().expect("type checked upstream");
            }
            "max-size-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.max_size = value.get::<u64>().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "max-alloc-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_alloc.to_value()
            }
            "max-size-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.max_size.to_value()
            }
            name => panic!("No getter for {name}"),
        }
    }
}

impl GstObjectImpl for ImageRsDecoder {}

impl ElementImpl for ImageRsDecoder {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "image-rs decoder (still formats)",
                "Codec/Decoder/Image",
                "Decodes still image formats",
                "Amyspark <amy@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let mut caps = gst::Caps::new_empty();
            {
                let caps = caps.get_mut().unwrap();

                for f in Format::all_decoding_formats() {
                    for v in f.to_mimetypes() {
                        caps.append(gst::Caps::new_empty_simple(v));
                    }
                }
            }

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::Rgb,
                    gst_video::VideoFormat::Rgba,
                    gst_video::VideoFormat::Gray8,
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Gray16Le,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Gray16Be,
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Rgba64Le,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Rgba64Be,
                ])
                .width_range(1..i32::MAX)
                .height_range(1..i32::MAX)
                .framerate(gst::Fraction::new(0, i32::MAX))
                .build();

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

        let mut state = self.state.lock().unwrap();

        if transition == gst::StateChange::ReadyToPaused {
            /* default to single image mode, setcaps function might not be called */
            state.duration = None;
            state.in_fps = None;
            state.in_par = None;
            state.info = None;
        }

        let v = self.parent_change_state(transition)?;

        if transition == gst::StateChange::PausedToReady {
            *state = Default::default();
        }

        Ok(v)
    }
}
