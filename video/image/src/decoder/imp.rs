// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on gstpixbufdec

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};

use std::collections::VecDeque;
use std::io::{BufRead, Cursor, Seek};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::utils;

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
    format_from_caps: Option<image::ImageFormat>,
    total_size: usize,
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

// Missing formats from gdkpixbufdec:
// - application/x-navi-animation
// - image/x-cmu-raster
// - image/x-sun-raster
// - image/svg
// - image/svg+xml
fn mimetypes() -> impl IntoIterator<Item = &'static str> {
    [
        // FIXME upstream: AVIF also supports animations
        // but needs image-rs support
        #[cfg(feature = "avif")]
        "image/avif",
        #[cfg(any(feature = "bmp", feature = "ico"))]
        "image/bmp",
        #[cfg(any(feature = "bmp", feature = "ico"))]
        "image/x-MS-bmp",
        #[cfg(feature = "dds")]
        "image/vnd-ms.dds",
        #[cfg(feature = "dds")]
        "image/x-direct-draw-surface",
        #[cfg(feature = "exr")]
        "image/exr",
        #[cfg(feature = "ff")]
        "image/x-farbfeld",
        #[cfg(feature = "ico")]
        "image/x-icon",
        #[cfg(feature = "jpeg")]
        // FIXME upstream: doesn't support MJPEG
        "image/jpeg",
        #[cfg(feature = "ora")]
        "image/openraster",
        // https://snisurset.net/code/abydos/supported.html
        #[cfg(feature = "otb")]
        "image/x-nokia-over-the-air-bitmap",
        #[cfg(feature = "pcx")]
        "image/x-pcx",
        #[cfg(any(feature = "png", feature = "ico"))]
        "image/png",
        #[cfg(feature = "pnm")]
        "image/x-portable-anymap",
        #[cfg(feature = "pnm")]
        "image/x-portable-bitmap",
        #[cfg(feature = "pnm")]
        "image/x-portable-graymap",
        #[cfg(feature = "pnm")]
        "image/x-portable-pixmap",
        // https://github.com/phoboslab/qoi/issues/167
        #[cfg(feature = "qoi")]
        "image/qoi",
        #[cfg(feature = "qoi")]
        "image/x-qoi",
        #[cfg(feature = "sgi")]
        "image/sgi",
        #[cfg(feature = "tga")]
        "image/x-targa",
        #[cfg(feature = "tga")]
        "image/x-tga",
        #[cfg(feature = "tiff")]
        "image/tiff",
        #[cfg(feature = "wbmp")]
        "image/vnd.wap.wbmp",
        #[cfg(feature = "xbm")]
        "image/x-xbitmap",
        #[cfg(feature = "xbm")]
        "image/x-xbm",
        #[cfg(feature = "xpm")]
        "image/x-xpixmap",
    ]
}

struct Wrapper(DynamicImage);

impl AsRef<[u8]> for Wrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl ImageRsDecoder {
    fn dec_chain(
        &self,
        pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj = pad, "Handling buffer {:?}", buffer);

        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let timestamp = buffer.pts();

        gst::log!(CAT, imp = self, "buffer with ts: {timestamp:?}");

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

    fn convert_format_and_strides(
        &self,
        image: DynamicImage,
    ) -> (DynamicImage, gst_video::VideoFormat, (usize, usize, usize)) {
        match image {
            #[cfg(target_endian = "little")]
            DynamicImage::ImageRgb8(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Rgb, strides)
            }
            #[cfg(target_endian = "big")]
            DynamicImage::ImageRgb8(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Bgr, strides)
            }
            #[cfg(target_endian = "little")]
            DynamicImage::ImageRgba8(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Rgba, strides)
            }
            #[cfg(target_endian = "big")]
            DynamicImage::ImageRgba8(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Rgba, strides)
            }
            DynamicImage::ImageLuma8(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Gray8, strides)
            }
            #[cfg(target_endian = "little")]
            DynamicImage::ImageLuma16(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Gray16Le, strides)
            }
            #[cfg(target_endian = "big")]
            DynamicImage::ImageLuma16(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Gray16Be, strides)
            }
            #[cfg(target_endian = "little")]
            DynamicImage::ImageRgba16(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Rgba64Le, strides)
            }
            #[cfg(target_endian = "big")]
            DynamicImage::ImageRgba16(ref p) => {
                let strides = p.as_flat_samples().strides_cwh();
                (image, gst_video::VideoFormat::Rgba64Be, strides)
            }
            v => {
                gst::debug!(
                    CAT,
                    imp = self,
                    "Format {:?} not supported, converting to RGBA",
                    v.color()
                );
                let image_rgba8 = v.to_rgba8();
                let fmt = if cfg!(target_endian = "little") {
                    gst_video::VideoFormat::Rgba
                } else {
                    gst_video::VideoFormat::Abgr
                };
                let strides = image_rgba8.as_flat_samples().strides_cwh();

                (DynamicImage::from(image_rgba8), fmt, strides)
            }
        }
    }

    fn set_format_from_caps(&self, caps: &gst::event::Caps) -> Result<(), gst::ErrorMessage> {
        let mime = caps.structure().unwrap();
        let mut state = self.state.lock().unwrap();
        match mime.name().as_str() {
            #[cfg(feature = "avif")]
            "image/avif" => state.format_from_caps = Some(image::ImageFormat::Avif),

            // The ICO format support enables PNG and BMP as transitive deps
            #[cfg(any(feature = "bmp", feature = "ico"))]
            "image/bmp" | "image/x-MS-bmp" => {
                state.format_from_caps = Some(image::ImageFormat::Bmp)
            }

            #[cfg(feature = "dds")]
            "image/vnd-ms.dds" | "image/x-direct-draw-surface" => {
                state.format_from_caps = Some(image::ImageFormat::Dds)
            }

            #[cfg(feature = "exr")]
            "image/x-exr" => state.format_from_caps = Some(image::ImageFormat::OpenExr),

            #[cfg(feature = "ff")]
            "image/x-farbfeld" => state.format_from_caps = Some(image::ImageFormat::Farbfeld),

            #[cfg(feature = "hdr")]
            "image/vnd.radiance" => state.format_from_caps = Some(ImageFormat::Hdr),

            #[cfg(feature = "ico")]
            "image/x-icon" => state.format_from_caps = Some(ImageFormat::Ico),

            #[cfg(feature = "jpeg")]
            "image/jpeg" => state.format_from_caps = Some(ImageFormat::Jpeg),

            #[cfg(feature = "ora")]
            "image/openraster" => state.format_from_caps = None,

            #[cfg(feature = "otb")]
            "image/x-nokia-over-the-air-bitmap" => state.format_from_caps = None,

            #[cfg(any(feature = "png", feature = "ico"))]
            "image/png" => state.format_from_caps = Some(ImageFormat::Png),

            #[cfg(feature = "pnm")]
            "image/x-portable-anymap"
            | "image/x-portable-bitmap"
            | "image/x-portable-graymap"
            | "image/x-portable-pixmap" => state.format_from_caps = Some(ImageFormat::Pnm),

            #[cfg(feature = "qoi")]
            "image/qoi" | "image/x-qoi" => state.format_from_caps = Some(ImageFormat::Qoi),

            #[cfg(feature = "sgi")]
            "image/sgi" => state.format_from_caps = None,

            #[cfg(feature = "tga")]
            "image/x-targa" | "image/x-tga" => state.format_from_caps = Some(ImageFormat::Tga),

            #[cfg(feature = "tiff")]
            "image/tiff" => state.format_from_caps = Some(ImageFormat::Tiff),

            #[cfg(feature = "wbmp")]
            "image/vnd.wap.wbmp" => state.format_from_caps = None,

            #[cfg(feature = "xbm")]
            "image/x-xbitmap" | "image/x-xbm" => state.format_from_caps = None,

            #[cfg(feature = "xpm")]
            "image/x-xpixmap" => state.format_from_caps = None,

            v => {
                return Err(gst::error_msg!(
                    gst::StreamError::CodecNotFound,
                    ["Unknown mimetype {v}"]
                ));
            }
        };
        state.in_fps = match mime.get::<gst::Fraction>("framerate") {
            Ok(v) => {
                gst::debug!(
                    CAT,
                    imp = self,
                    "got framerate of {} fps => packetized mode",
                    v,
                );
                v.into()
            }
            Err(v) => {
                gst::debug!(
                    CAT,
                    imp = self,
                    // FIXME: this needs changing in gdkpixbufdec too
                    "no framerate available: {v:?}"
                );
                None
            }
        };
        state.in_par = match mime.get::<gst::Fraction>("pixel-aspect-ratio") {
            Ok(v) => v.into(),
            Err(v) => {
                gst::debug!(CAT, imp = self, "no pixel aspect ratio found: {v:?}");
                None
            }
        };

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

        reader = match state.format_from_caps {
            Some(v) => {
                reader.set_format(v);
                reader
            }
            None => reader.with_guessed_format().map_err(|v| {
                gst::error!(
                    CAT,
                    imp = self,
                    "No caps available, failed guessing format: {v}"
                );
                gst::FlowError::NotNegotiated
            })?,
        };

        let mut limits = Limits::default();
        {
            if settings.max_alloc != 0 {
                limits.max_alloc = Some(settings.max_alloc);
            }
        }
        reader.limits(limits);

        drop(settings);

        let mut decoder = reader.into_decoder().map_err(|v| {
            gst::error!(CAT, imp = self, "Failed decoding single image: {v}");
            gst::FlowError::Error
        })?;

        let metadata = self.metadata_from_decoder(&mut decoder);

        let image = DynamicImage::from_decoder(decoder).map_err(|v| {
            gst::error!(CAT, imp = self, "Failed decoding single image: {v}");
            gst::FlowError::Error
        })?;

        let wh = image.dimensions();

        let (image, fmt, strides) = self.convert_format_and_strides(image);

        let pending_events = if state.info.is_none() {
            gst::debug!(CAT, imp = self, "Set size to {}x{}", wh.0, wh.1);
            let fps = state.in_fps;
            let par = state.in_par;

            let strides: [i32; 4] = [strides.2.try_into().unwrap(), 0, 0, 0];

            let color_info = match utils::cicp_to_videoinfo(image.color_space()) {
                Ok(v) => Some(v),
                Err(v) => {
                    gst::warning!(CAT, imp = self, "Failed converting to VideoInfo: {v}");
                    None
                }
            };

            let info = gst_video::VideoInfo::builder(fmt, wh.0, wh.1)
                .fps_if_some(fps)
                .par_if_some(par)
                .stride(&strides)
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
                })?;

            let caps = &info.to_caps().unwrap();

            state.info = Some(info);

            let pending_events: Vec<_> = state.pending_events.drain(..).collect();

            drop(state);

            let _ = self.srcpad.push_event(gst::event::Caps::new(caps));

            pending_events
        } else {
            drop(state);
            vec![]
        };

        for l in pending_events {
            self.srcpad.push_event(l);
        }

        // FIXME: this should be validated
        // assert_eq!(state.info.as_ref().unwrap().format(), fmt);

        let mut outbuf = gst::Buffer::from_slice(Wrapper(image));
        {
            let outbuf = outbuf.get_mut().unwrap();
            outbuf.set_pts(timestamp);
            outbuf.set_duration(None);
        }

        gst::debug!(CAT, imp = self, "pushing... {} bytes", outbuf.size());

        if metadata.n_tags() > 0 {
            let v = gst::event::Tag::new(metadata);
            self.srcpad.push_event(v);
        }

        match self.srcpad.push(outbuf) {
            Ok(_) => (),
            Err(flow) => {
                gst::error!(CAT, imp = self, "Failed to push buffers: {flow:?}");
                return Err(flow);
            }
        }

        Ok(gst::FlowSuccess::Ok)
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

    fn get_capslist(&self, filter: Option<&gst::CapsRef>) -> gst::Caps {
        let mut capslist = gst::Caps::new_empty();
        {
            let capslist = capslist.get_mut().unwrap();
            for mime in mimetypes() {
                capslist.append_structure(gst::Structure::new_empty(mime));
            }
        }

        let tmpl_caps = ImageRsDecoder::pad_templates()[1].caps();
        let mut return_caps = capslist.intersect(tmpl_caps);

        if let Some(f) = filter
            && !return_caps.is_empty()
        {
            return_caps = return_caps.intersect(f);
        }

        return_caps
    }

    fn sink_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                let filter = q.filter();
                let caps = self.get_capslist(filter);
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
                        forward = false;
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

        image_extras::register();
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

                for mimetype in mimetypes() {
                    caps.append(gst::Caps::new_empty_simple(mimetype));
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
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Rgb,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Bgr,
                    #[cfg(target_endian = "little")]
                    gst_video::VideoFormat::Rgba,
                    #[cfg(target_endian = "big")]
                    gst_video::VideoFormat::Abgr,
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
