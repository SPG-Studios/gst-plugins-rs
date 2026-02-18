// SPDX-CopyrightText: 2026 Amyspark <amy@centricular.com>
// SPDX-License-Identifier: MPL-2.0
// Based on Mathieu Duponchelle's WebP plugin -- see video/webp/src/dec/imp.rs

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use image::Limits;
use image_extras;
#[cfg(any(feature = "gif", feature = "webp"))]
use image::{AnimationDecoder, Frame, ImageDecoder};
use image::{DynamicImage, GenericImageView, ImageFormat, ImageReader};
#[cfg(any(feature = "gif", feature = "webp"))]
use num_rational::Ratio;

#[cfg(feature = "gif")]
use image::codecs::gif::GifDecoder;
#[cfg(feature = "webp")]
use image::codecs::webp::WebPDecoder;

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{LazyLock, Mutex, MutexGuard};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ImageRsDecoder",
        gst::DebugColorFlags::empty(),
        Some("image-rs decoder"),
    )
});

#[derive(Default)]
struct Settings {
    max_size: u64,
    max_alloc: u64,
}

#[derive(Default)]
struct State {
    last_timestamp: Option<gst::ClockTime>,
    buffers: Vec<gst::Buffer>,
    caps: Option<gst::Caps>,
    format_from_caps: Option<image::ImageFormat>,
    total_size: usize,
    in_fps: (i32, i32),
    info: Option<gst_video::VideoInfo>,
    pool: Option<gst::BufferPool>,
    pending_events: VecDeque<gst::Event>,
    packetized: bool,
}

#[cfg(any(feature = "gif", feature = "webp"))]
struct AnimatedImageWrapper(Frame);

#[cfg(any(feature = "gif", feature = "webp"))]
impl AsRef<[u8]> for AnimatedImageWrapper {
    fn as_ref(&self) -> &[u8] {
        self.0.buffer()
    }
}

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
        #[cfg(feature = "gif")]
        "image/gif",
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
        #[cfg(feature = "webp")]
        "image/webp",
        #[cfg(feature = "xbm")]
        "image/x-xbitmap",
        #[cfg(feature = "xbm")]
        "image/x-xbm",
        #[cfg(feature = "xpm")]
        "image/x-xpixmap",
    ]
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
        match timestamp {
            Some(v) => state.last_timestamp = Some(v),
            _ => {}
        };

        gst::log!(CAT, imp = self, "buffer with ts: {timestamp:?}");

        if settings.max_size == 0 || (state.total_size + buffer.size()) as u64 <= settings.max_size
        {
            gst::log!(CAT, imp = self, "Writing buffer size {}", buffer.size());
            state.total_size += buffer.size();
            state.buffers.push(buffer);

            if state.packetized {
                return self.decode(settings, state);
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

    fn setup_pool<'a>(&'a self, state: &mut MutexGuard<'a, State>) -> Result<(), gst::FlowError> {
        /* try to get a bufferpool now */
        /* find a pool for the negotiated caps now */
        let mut pool: Option<gst::BufferPool>;
        let size: u32;
        let min: u32;
        let max: u32;

        if let Some(v) = state.caps.as_ref() {
            let mut query = gst::query::Allocation::new(Some(&v), true);
            if !self.srcpad.peer_query(query.query_mut()) {
                /* not a problem, we use the query defaults */
                gst::debug!(CAT, imp = self, "ALLOCATION query failed");
            }

            match query.allocation_pools().nth(0) {
                Some(v) => {
                    /* we got configuration from our peer, parse them */
                    pool = v.0;
                    size = v.1;
                    min = v.2;
                    max = v.3;
                }
                None => {
                    pool = None;
                    size = state.info.as_ref().unwrap().size().try_into().unwrap();
                    min = 0;
                    max = 0;
                }
            }
        } else {
            gst::element_error!(
                self.obj(),
                gst::StreamError::Failed,
                ["Cannot allocate buffer pool"]
            );
            return Err(gst::FlowError::Error);
        }

        if pool == None {
            /* we did not get a pool, make one ourselves then */
            pool = Some(gst::BufferPool::new());
        }

        let mut config = pool.as_ref().unwrap().config();
        config.set_params(state.caps.as_ref(), size, min, max);
        pool.as_ref()
            .expect("Buffer must be inactive")
            .set_config(config)
            .unwrap();

        if let Some(v) = state.pool.as_ref() {
            let _ = v.set_active(false);
            state.pool = None;
        }
        state.pool = pool;

        /* and activate */
        state.pool.as_ref().unwrap().set_active(true).unwrap();

        Ok(())
    }

    fn render_single_frame<'a>(
        &'a self,
        image: DynamicImage,
        mut state: MutexGuard<'a, State>,
        settings: MutexGuard<'a, Settings>
    ) -> Result<(), gst::FlowError> {
        let wh = image.dimensions();

        let mut needs_conversion = false;

        let fmt = match image.color() {
            #[cfg(target_endian = "little")]
            image::ColorType::Rgb8 => gst_video::VideoFormat::Rgb,
            #[cfg(target_endian = "big")]
            image::ColorType::Rgb8 => gst_video::VideoFormat::Bgr,
            #[cfg(target_endian = "little")]
            image::ColorType::Rgba8 => gst_video::VideoFormat::Rgba,
            #[cfg(target_endian = "big")]
            image::ColorType::Rgba8 => gst_video::VideoFormat::Bgra,
            image::ColorType::L8 => gst_video::VideoFormat::Gray8,
            #[cfg(target_endian = "little")]
            image::ColorType::L16 => gst_video::VideoFormat::Gray16Le,
            #[cfg(target_endian = "big")]
            image::ColorType::L16 => gst_video::VideoFormat::Gray16Be,
            #[cfg(target_endian = "little")]
            image::ColorType::Rgba16 => gst_video::VideoFormat::Rgba64Le,
            #[cfg(target_endian = "big")]
            image::ColorType::Rgba16 => gst_video::VideoFormat::Rgba64Be,
            v => {
                gst::element_warning!(
                    self.obj(),
                    gst::StreamError::Decode,
                    ["Format {:?} not supported, converting to RGBA", v]
                );
                needs_conversion = true;

                if cfg!(target_endian = "little") {
                    gst_video::VideoFormat::Rgba
                } else {
                    gst_video::VideoFormat::Bgra
                }
            }
        };

        if state.info.is_none() {
            gst::debug!(CAT, imp = self, "Set size to {}x{}", wh.0, wh.1);
            let fps = state.in_fps;

            let info = gst_video::VideoInfo::builder(fmt, wh.0, wh.1)
                .fps(fps)
                .build()
                .map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        [
                            "Format {} with {}x{} @ {:?} not supported: {v}",
                            fmt,
                            wh.0,
                            wh.1,
                            fps
                        ]
                    );
                    gst::FlowError::Error
                })?;

            state.info = Some(info);

            {
                let caps = state.info.as_ref().unwrap().to_caps().unwrap();
                let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
                state.caps = Some(caps);
            }

            self.setup_pool(&mut state)?;

            for l in state.pending_events.drain(..) {
                self.srcpad.push_event(l);
            }
        }

        // FIXME: this should be validated
        assert_eq!(state.info.as_ref().unwrap().format(), fmt);

        let mut outbuf = state.pool.as_ref().unwrap().acquire_buffer(None)?;

        {
            let outbuf = outbuf.get_mut().unwrap();
            outbuf.set_pts(state.last_timestamp);
            outbuf.set_duration(None);

            if needs_conversion {
                let image_rgba8 = image.to_rgba8();
                if let Err(v) = outbuf.copy_from_slice(0, image_rgba8.as_raw()) {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        [
                            "Mismatched buffer size: image {:?}, copied {v} bytes",
                            image_rgba8.as_flat_samples().extents()
                        ]
                    );
                    return Err(gst::FlowError::Error);
                }
            } else {
                if let Err(v) = outbuf.copy_from_slice(0, image.as_bytes()) {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        [
                            "Mismatched buffer size: image {:?} {:?}, copied {v} bytes",
                            image.color(),
                            image.dimensions()
                        ]
                    );
                    return Err(gst::FlowError::Error);
                }
            }
        }

        gst::debug!(CAT, imp = self, "pushing... {} bytes", outbuf.size());

        drop(state);
        drop(settings);

        match self.srcpad.push(outbuf) {
            Ok(_) => (),
            Err(flow) => {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Failed,
                    ["Failed to push buffers: {:?}", flow]
                );
                return Err(flow);
            }
        }

        Ok(())
    }

    #[cfg(any(feature = "gif", feature = "webp"))]
    fn render_many_frames<'a>(
        &self,
        decoder: impl AnimationDecoder<'a> + ImageDecoder,
    ) -> Result<(), gst::ErrorMessage> {
        let mut prev_timestamp = gst::ClockTime::ZERO;
        let wh = decoder.dimensions();

        let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, wh.0, wh.1)
            .fps((0, 1))
            .build()
            .unwrap()
            .to_caps()
            .unwrap();

        let segment = gst::FormattedSegment::<gst::ClockTime>::new();

        let _ = self.srcpad.push_event(gst::event::Caps::new(&caps));
        let _ = self.srcpad.push_event(gst::event::Segment::new(&segment));

        // into_frames already blends the previous and current frames
        // see https://github.com/image-rs/image/blob/0779d359908cf9bf04cbd1998a1a9940e368cd56/src/codecs/gif.rs#L355
        for frame in decoder.into_frames() {
            let frame = frame.map_err(|v| {
                gst::error_msg!(
                    gst::StreamError::Decode,
                    ["Failed to get next frame: {}", v]
                )
            })?;

            let delay = {
                let d: Ratio<u32> = frame.delay().numer_denom_ms().into();
                (d.to_integer() as u64).mseconds()
            };

            // AnimatedEncoder doesn't support anything other than RGBA
            let mut out_buf = gst::Buffer::from_slice(AnimatedImageWrapper(frame));
            {
                let out_buf_mut = out_buf.get_mut().unwrap();
                out_buf_mut.set_pts(prev_timestamp);
                out_buf_mut.set_duration(delay);
            }

            prev_timestamp += delay;

            match self.srcpad.push(out_buf) {
                Ok(_) => (),
                Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => break,
                Err(flow) => {
                    return Err(gst::error_msg!(
                        gst::StreamError::Failed,
                        ["Failed to push buffers: {:?}", flow]
                    ));
                }
            }
        }

        Ok(())
    }

    fn set_format_from_caps(&self, caps: &gst::event::Caps) -> Result<(), gst::ErrorMessage> {
        match caps.structure() {
            Some(mime) => {
                let mut state = self.state.lock().unwrap();
                state.format_from_caps = None;
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
                    "image/x-farbfeld" => {
                        state.format_from_caps = Some(image::ImageFormat::Farbfeld)
                    }

                    #[cfg(feature = "gif")]
                    "image/gif" => state.format_from_caps = Some(image::ImageFormat::Gif),

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
                    "image/x-targa" | "image/x-tga" => {
                        state.format_from_caps = Some(ImageFormat::Tga)
                    }

                    #[cfg(feature = "tiff")]
                    "image/tiff" => state.format_from_caps = Some(ImageFormat::Tiff),

                    #[cfg(feature = "wbmp")]
                    "image/vnd.wap.wbmp" => state.format_from_caps = None,

                    #[cfg(feature = "webp")]
                    "image/webp" => state.format_from_caps = Some(ImageFormat::WebP),

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
                }
                Ok(())
            }
            None => Err(gst::error_msg!(
                gst::StreamError::Format,
                ["No mimetype available from caps, falling back to decoder sniffing"]
            )),
        }
    }

    fn decode<'a>(
        &'a self,
        settings: MutexGuard<'a, Settings>,
        mut state: MutexGuard<'a, State>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if state.buffers.is_empty() {
            gst::element_error!(self.obj(), gst::StreamError::Decode, ["No buffers found"]);
            return Err(gst::FlowError::Error);
        }

        let mut buf = Vec::with_capacity(state.total_size);

        for buffer in state.buffers.drain(..) {
            buf.extend_from_slice(&buffer.map_readable().expect("Failed to map buffer"));
        }
        state.total_size = 0;

        let cursor = Cursor::new(buf);
        let mut reader = ImageReader::new(cursor);

        reader = match state.format_from_caps {
            Some(v) => {
                reader.set_format(v);
                reader
            }
            None => reader.with_guessed_format().map_err(|v| {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Decode,
                    ["No caps available, failed guessing format: {v}"]
                );
                gst::FlowError::Error
            })?,
        };

        let mut limits = Limits::default();
        {
            if settings.max_alloc != 0 {
                limits.max_alloc = Some(settings.max_alloc);
            }
        }
        match reader.format() {
            #[cfg(feature = "gif")]
            Some(ImageFormat::Gif) => {
                let mut decoder = GifDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        ["Failed decoding GIF container: {v}"]
                    );
                    gst::FlowError::Error
                })?;
                decoder.set_limits(limits).map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        ["Failed setting memory limits: {v}"]
                    );
                    gst::FlowError::Error
                })?;

                self.render_many_frames(decoder)?;
            }
            #[cfg(feature = "webp")]
            Some(ImageFormat::WebP) => {
                let mut decoder = WebPDecoder::new(reader.into_inner()).map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        ["Failed decoding WebP container: {v}"]
                    );
                    gst::FlowError::Error
                })?;
                decoder.set_limits(limits).map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        ["Failed setting memory limits: {v}"]
                    );
                    gst::FlowError::Error
                })?;

                self.render_many_frames(decoder)?;
            }
            Some(_) => {
                reader.limits(limits);
                let image = reader.decode().map_err(|v| {
                    gst::element_error!(
                        self.obj(),
                        gst::StreamError::Decode,
                        ["Failed decoding single image: {v}"]
                    );
                    gst::FlowError::Error
                })?;
                self.render_single_frame(image, state, settings)?;
            }
            None => {
                gst::element_error!(
                    self.obj(),
                    gst::StreamError::Decode,
                    ["Failed reading for format detection"]
                );
                return Err(gst::FlowError::Error);
            }
        }

        Ok(gst::FlowSuccess::Ok)
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

        match filter {
            Some(f) => {
                if !return_caps.is_empty() {
                    return_caps = return_caps.intersect(f);
                }
            }
            None => {}
        }

        return_caps
    }

    fn query_event(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
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
            EventView::Eos(..) => {
                let settings = self.settings.lock().unwrap();
                let state = self.state.lock().unwrap();
                match self.decode(settings, state) {
                    Ok(_) => {}
                    Err(v) => match v {
                        gst::FlowError::Flushing
                        | gst::FlowError::Eos
                        | gst::FlowError::NotLinked => {}
                        _ => {
                            forward = false;
                            ret = false;
                        }
                    },
                };
            }
            EventView::FlushStop(..) => {
                let mut state = self.state.lock().unwrap();
                state.pending_events.clear();
            }
            EventView::Segment(v) => {
                let mut state = self.state.lock().unwrap();
                let segment = v.segment();
                state.packetized = segment.format() != gst::Format::Bytes;
                if segment.format() != gst::Format::Time {
                    let seqnum = event.seqnum();
                    let mut output_segment = gst::Segment::new();
                    output_segment.reset_with_format(gst::Format::Time);
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
                    |dec| dec.query_event(pad, query),
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
                "image-rs decoder",
                "Codec/Decoder/Video",
                "Decodes potentially animated images",
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

            let pixel = if cfg!(target_endian = "big") {
                gst_video::VideoFormat::Bgra
            } else {
                gst_video::VideoFormat::Argb
            };

            let caps = gst_video::VideoCapsBuilder::new()
                .format(pixel)
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
            state.in_fps = (0, 1);
            state.info = None;
        }

        let v = self.parent_change_state(transition)?;

        if transition == gst::StateChange::PausedToReady {
            state.in_fps = (0, 0);
            if let Some(pool) = &state.pool {
                let _ = pool.set_active(false);
                state.pool = None;
            }
            state.pending_events.clear();
            // FIXME: close reader here?
        }

        Ok(v)
    }
}
