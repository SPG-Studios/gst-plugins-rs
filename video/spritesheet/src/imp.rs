// Copyright (C) 2023 Daily.co <rajneesh@daily.co>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
* SECTION:element-spritesheet
* @symbols:
*   - GstSpriteSheet
*
* `spritesheet` plugin can generate spritesheet used for thumbnail seeking in the player.
* It is used along with webvtt file which define sparial-dimensions
* of media. https://www.w3.org/TR/media-frags/#naming-space
*
* example usage
* ``` bash
* gst-launch-1.0 -v filesrc location=<INPUT_MP4_FILE>  ! qtdemux name=d ! queue max-size-time=-1 ! video/x-h264  ! h264parse \
* ! avdec_h264 !  videoconvert  ! video/x-raw,format=RGB ! queue name=myq  !  \
* spritesheet tile-height=100  tile-width=100 num-columns=20  num-rows=20 num-skip-frames=10 ! videoconvert !  jpegenc ! \
* multifilesink location=OUTPUT_%05d.jpeg
* ```
*
* ## Details
*
* output width = tile_width * num_tile_columns
* output height = tile_height * num_rows
* it allocates a blank buffer (called canvas), on getting the input buffer, plugin
* generate the thumbnail and copy it to canvas, when canvas is full, plugin outputs the buffer.
* when it generates a tile, it also sends a message to application, message contains
* enough information to create entry in the webvtt file.
*
*/
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::EventView;
use gst_base::prelude::*;
use gst_base::subclass::base_transform::GenerateOutputSuccess;
use gst_base::subclass::prelude::*;
use gst_video::VideoInfo;
use gst_video::{VideoConverter, VideoConverterConfig, VideoFormat, VideoFrame};
use once_cell::sync::Lazy;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "spritesheet",
        gst::DebugColorFlags::empty(),
        Some("spritesheet generation"),
    )
});

const DEFAULT_NUM_SKIP_FRAMES: i64 = 100;
const DEFAULT_NUM_COLUMNS: u32 = 10;
const DEFAULT_NUM_ROWS: u32 = 10;

#[derive(Debug)]
struct Settings {
    num_skip_frames: i64,
    tile_height: u32,
    tile_width: u32,
    num_columns: u32,
    num_rows: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            num_skip_frames: DEFAULT_NUM_SKIP_FRAMES,
            tile_height: 0,
            tile_width: 0,
            num_columns: DEFAULT_NUM_COLUMNS,
            num_rows: DEFAULT_NUM_ROWS,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Default)]
enum State {
    #[default]
    Stopped,
    Started(SpriteState),
}

// to save tile data for message
#[derive(Debug, Default)]
struct TileData {
    start_ts: Option<gst::ClockTime>,
    running_time: Option<gst::ClockTime>,
    stream_time: Option<gst::ClockTime>,
    x_pos: u32,
    y_pos: u32,
    canvas_idx: i64,
}

#[derive(Debug, Default)]
struct SpriteState {
    frame_count: i64,
    canvas_count: i64,
    col_count: u32,
    row_count: u32,
    is_canvas_full: bool,
    prev_tile_data: Option<TileData>,
    canvas_start_ts: Option<gst::ClockTime>,
    canvas_end_ts: Option<gst::ClockTime>,
    tile_duration: Option<gst::ClockTime>,
    in_info: Option<gst_video::VideoInfo>,
    out_info: Option<gst_video::VideoInfo>,
    canvas: Option<gst::Buffer>,
}

impl SpriteState {
    fn update_coordinates_for_next_tile(&mut self, settings: &Settings) -> Result<(), &str> {
        assert!(!self.is_canvas_full);
        let mut is_last_column = false;
        let mut is_last_row = false;

        self.col_count += 1;
        if self.col_count >= settings.num_columns {
            is_last_column = true;
            self.col_count = 0;
            self.row_count += 1;
        }

        if self.row_count >= settings.num_rows {
            is_last_row = true;
            self.row_count = 0;
        }

        self.is_canvas_full = is_last_row && is_last_column;

        Ok(())
    }

    fn reset_tile_tracking(&mut self) {
        self.is_canvas_full = false;
        self.col_count = 0;
        self.row_count = 0;
        self.canvas_start_ts.take();
        self.canvas_end_ts.take();
        self.tile_duration.take();
    }

    fn prepare_output_buffer(&mut self) -> Result<Option<gst::Buffer>, &str> {
        // Tiles are arranged like below on canvas.
        // |--T0--||--T1--|
        // |--T2--||--T3--|
        //
        // start_pts = pts T0
        // end_pts = pts T3
        // (start_pts - end_pts) = duration of T0+T1+T2
        // duration_of_last_tile = duration of T3

        if let Some(mut obuf) = self.canvas.take() {
            let duration = self
                .canvas_end_ts
                .opt_checked_sub(self.canvas_start_ts)
                .ok()
                .flatten()
                .opt_checked_add(self.tile_duration)
                .ok()
                .flatten();

            let mut_obuf = obuf
                .get_mut()
                .ok_or(|| {})
                .map_err(|_| "Failed to get mutable ref to output buffer")?;

            mut_obuf.set_pts(self.canvas_start_ts);
            mut_obuf.set_duration(duration);

            Ok(Some(obuf))
        } else {
            Ok(None)
        }
    }
}

#[derive(Default)]
pub struct SpriteSheet {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl SpriteSheet {
    fn drain(self: &SpriteSheet) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        let state = match *state {
            State::Started(ref mut state) => state,
            State::Stopped => {
                unreachable!("EOS even before state change")
            }
        };
        // post message about the last tile in the canvas
        let tile_data = state.prev_tile_data.take();
        if let Some(tile_data) = tile_data {
            self.post_tile_data_message(
                tile_data,
                gst::ClockTime::ZERO,
                settings.tile_width,
                settings.tile_height,
            );
        }

        let obuf = state.prepare_output_buffer().map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to prepare output buffer. {:?}", e);
            gst::FlowError::Error
        })?;

        state.reset_tile_tracking();
        if let Some(obuf) = obuf {
            state.canvas_count += 1;
            self.obj().src_pad().push(obuf)
        } else {
            gst::debug!(CAT, imp: self, "No pending buffer");
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn post_tile_data_message(
        self: &SpriteSheet,
        tile_data: TileData,
        end_ts: gst::ClockTime,
        tile_width: u32,
        tile_height: u32,
    ) {
        if let Err(e) = self.obj().post_message(
            gst::message::Element::builder(
                gst::structure::Structure::builder("spritesheet-tile-data")
                    .field("canvas-idx", tile_data.canvas_idx)
                    .field("start-ts", tile_data.start_ts)
                    .field("end-ts", end_ts)
                    .field("tile-width", tile_width)
                    .field("tile-height", tile_height)
                    .field("tile-x", tile_data.x_pos)
                    .field("tile-y", tile_data.y_pos)
                    .field("running-time", tile_data.running_time)
                    .field("stream-time", tile_data.stream_time)
                    .build(),
            )
            .src(&*self.obj())
            .build(),
        ) {
            gst::error!(CAT, "Failed to post message on bus. {:?}", e);
        };
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SpriteSheet {
    const NAME: &'static str = "GstSpriteSheet";
    type Type = super::SpriteSheet;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for SpriteSheet {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecInt64::builder("num-skip-frames")
                    .nick("num-skip-frames")
                    .blurb("the number of frames to skip between tiles in spritesheet, -1 for single tile.")
                    .minimum(-1)
                    .maximum(i64::MAX)
                    .default_value(DEFAULT_NUM_SKIP_FRAMES)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("tile-height")
                    .nick("height")
                    .blurb("height of single tile in sprite, 0 means calculate based on tile width. Both width and height 0 means same as input")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("tile-width")
                    .nick("width")
                    .blurb("width of single tile in sprite, 0 means calculate based on tile height.Both width and height 0 means same as input image")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("num-columns")
                    .nick("columns")
                    .blurb("number of columns in sprite")
                    .minimum(1)
                    .maximum(120)
                    .default_value(DEFAULT_NUM_ROWS)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("num-rows")
                    .nick("rows")
                    .blurb("number of rows in sprite")
                    .minimum(1)
                    .maximum(120)
                    .default_value(DEFAULT_NUM_COLUMNS)
                    .mutable_ready()
                    .build(),
                //TODO: some other properties like maintain aspect ratio,
                //      scaling quality, start offset(initial frames to skip)
                //      unit property time, frames, to control the interval
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = self.settings.lock().unwrap();

        match pspec.name() {
            "num-skip-frames" => {
                settings.num_skip_frames = value.get::<i64>().unwrap();
                gst::info!(
                    CAT,
                    imp: self,
                    "setting property {} : {}",
                    pspec.name(),
                    settings.num_skip_frames
                );
            }
            "tile-height" => {
                settings.tile_height = value.get::<u32>().unwrap();
                gst::info!(
                    CAT,
                    imp: self,
                    "setting property {} : {}",
                    pspec.name(),
                    settings.tile_height
                );
            }
            "tile-width" => {
                settings.tile_width = value.get::<u32>().unwrap();
                gst::info!(
                    CAT,
                    imp: self,
                    "setting property {} : {}",
                    pspec.name(),
                    settings.tile_width
                );
            }
            "num-columns" => {
                settings.num_columns = value.get::<u32>().unwrap();
                gst::info!(
                    CAT,
                    imp: self,
                    "setting property {} : {}",
                    pspec.name(),
                    settings.num_columns
                );
            }
            "num-rows" => {
                settings.num_rows = value.get::<u32>().unwrap();
                gst::info!(
                    CAT,
                    imp: self,
                    "setting property {} : {}",
                    pspec.name(),
                    settings.num_rows
                );
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "num-skip-frames" => settings.num_skip_frames.to_value(),
            "tile-height" => settings.tile_height.to_value(),
            "tile-width" => settings.tile_width.to_value(),
            "num-columns" => settings.num_columns.to_value(),
            "num-rows" => settings.num_rows.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for SpriteSheet {}

impl ElementImpl for SpriteSheet {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Sprite sheet generation",
                "Filter/Video/spritesheet",
                "Generate sprite sheets for the input video frames",
                "Rajneesh <rajneesh@daily.co>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    VideoFormat::Rgb,
                    VideoFormat::Rgbx,
                    VideoFormat::Bgr,
                    VideoFormat::Bgrx,
                ])
                .build();

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
}

impl BaseTransformImpl for SpriteSheet {
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;

    fn submit_input_buffer(
        &self,
        _is_discont: bool,
        inbuf: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // FIXME: is_discont is `true` for all the frames. So using buffer flags.
        // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/2589
        if inbuf.flags().contains(gst::BufferFlags::DISCONT) {
            gst::info!(CAT, "Discontinuous input flush the canvas.");
            self.drain().map_err(|_e| gst::FlowError::Error)?;
        }

        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut state) => state,
            State::Stopped => {
                unreachable!("input buffer submitted without starting")
            }
        };

        // skip this frame, based on periodicity of the tile generation
        if (settings.num_skip_frames == -1 && state.frame_count != 0)
            || (state.frame_count % settings.num_skip_frames != 0)
        {
            state.frame_count += 1;
            return Ok(gst::FlowSuccess::Ok);
        }
        // A tile is put every "num_skip_frames", so one tile represent the
        // span of "num_skip_frames". The total duration of tile data
        // represents (start_time_of_next_tile - start_time_of_curr_tile)
        // hence message can be posted only when we know the pts of next tile.
        let tile_data = state.prev_tile_data.take();
        let inbuf_pts = inbuf.pts();
        if let Some(tile_data) = tile_data {
            self.post_tile_data_message(
                tile_data,
                inbuf_pts.unwrap_or(gst::ClockTime::ZERO),
                settings.tile_width,
                settings.tile_height,
            );
        };

        let out_info = state
            .out_info
            .as_ref()
            .expect("input frame submitted without set_caps");

        let in_info = state
            .in_info
            .as_ref()
            .expect("input frame submitted without set_caps");

        let curr_buf = state.canvas.take();
        // Allocate the output Frame to overlay the tile
        let obuf = match curr_buf {
            Some(buffer) => {
                state.canvas_end_ts = inbuf_pts;
                state.tile_duration = inbuf.duration();
                Ok(buffer)
            }
            None => {
                state.canvas_start_ts = inbuf_pts;
                state.canvas_end_ts = inbuf_pts;
                state.tile_duration = inbuf.duration();
                gst::Buffer::with_size(out_info.size())
            }
        }
        .map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to allocate output buffer. {:?}", e);
            gst::FlowError::Error
        })?;

        let mut oframe = VideoFrame::from_buffer_writable(obuf, out_info).map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to map output buffer. {:?}", e);
            gst::FlowError::Error
        })?;

        let iframe = VideoFrame::from_buffer_readable(inbuf, in_info).map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to map input buffer. {:?}", e);
            gst::FlowError::Error
        })?;

        let mut config = VideoConverterConfig::new();
        let dest_x = state.col_count * settings.tile_width;
        let dest_y = state.row_count * settings.tile_height;

        config.set_dest_x(dest_x as i32);
        config.set_dest_y(dest_y as i32);
        config.set_dest_width(Some(settings.tile_width as i32));
        config.set_dest_height(Some(settings.tile_height as i32));
        if dest_x == 0 && dest_y == 0 {
            config.set_fill_border(true);
        } else {
            config.set_fill_border(false);
        }
        // FIXME: We can cache the videoconvert instance and use set_config() to update
        // dst_x and dst_y, but set_config is not working.
        // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/2590
        let convert = VideoConverter::new(in_info, out_info, Some(config)).map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to convert frame. {:?}", e);
            gst::FlowError::Error
        })?;
        convert.frame(&iframe, &mut oframe);

        let segment = self.obj().segment().downcast::<gst::ClockTime>().ok();
        let running_time = segment.as_ref().and_then(|s| s.to_running_time(inbuf_pts));
        let stream_time = segment.as_ref().and_then(|s| s.to_stream_time(inbuf_pts));

        state.prev_tile_data = Some(TileData {
            start_ts: inbuf_pts,
            running_time,
            stream_time,
            x_pos: dest_x,
            y_pos: dest_y,
            canvas_idx: state.canvas_count,
        });
        // update spriteState
        state.frame_count += 1;
        state
            .update_coordinates_for_next_tile(&settings)
            .map_err(|e| {
                gst::error!(CAT, imp: self, "coordinate update failed. {:?}", e);
                gst::FlowError::Error
            })?;

        state.canvas = Some(oframe.into_buffer());

        Ok(gst::FlowSuccess::Ok)
    }

    fn generate_output(&self) -> Result<GenerateOutputSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut state) => state,
            State::Stopped => {
                unreachable!("input buffer submitted without starting")
            }
        };

        if !state.is_canvas_full {
            return Ok(GenerateOutputSuccess::NoOutput);
        }

        let obuf = state.prepare_output_buffer().map_err(|e| {
            gst::error!(CAT, imp: self, "Failed to prepare output buffer. {:?}", e);
            gst::FlowError::Error
        })?;

        if let Some(obuf) = obuf {
            gst::debug!(CAT, imp: self, "send output buffer: {:?}", obuf);
            state.reset_tile_tracking();
            state.canvas_count += 1;
            Ok(GenerateOutputSuccess::Buffer(obuf))
        } else {
            Ok(GenerateOutputSuccess::NoOutput)
        }
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        match event.view() {
            EventView::FlushStop(_) => {
                gst::debug!(CAT, imp: self, "Reset data on FlushStop");

                let mut state = self.state.lock().unwrap();
                match *state {
                    State::Started(ref mut state) => {
                        state.frame_count = 0;
                        state.canvas_count = 0;
                        state.canvas.take();
                        state.reset_tile_tracking();
                    }
                    State::Stopped => {
                        gst::info!(CAT, "FlushStop in stopped state");
                    }
                };
            }
            EventView::Eos(_) => {
                gst::debug!(CAT, imp: self, "Handle flushing the last buffer on EOS");
                // Do we need to reset the state ? reset in flushStop should be enough.
                if self.drain().is_err() {
                    gst::warning!(CAT, "Failed to push last buffer on EOS");
                }
            }
            _ => {}
        }

        self.parent_sink_event(event)
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        caps: &gst::Caps,
        filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let other_caps = if direction == gst::PadDirection::Src {
            caps.clone()
        } else {
            let mut output_caps = gst::Caps::new_empty();
            {
                let output_caps = output_caps.get_mut().unwrap();

                for s in caps.iter() {
                    let mut s_output = s.to_owned();
                    let in_height = s.get::<i32>("height");
                    let in_width = s.get::<i32>("width");
                    // if width/height is not a range, plugin can determine the output width and height
                    if in_width.is_ok() && in_height.is_ok() {
                        let mut settings = self.settings.lock().unwrap();
                        let in_width = in_width.unwrap();
                        let in_height = in_height.unwrap();

                        (settings.tile_width, settings.tile_height) = get_tile_width_height(
                            settings.tile_width,
                            settings.tile_height,
                            in_height,
                            in_width,
                        );
                        let out_width = (settings.num_columns * settings.tile_width) as i32;
                        let out_height = (settings.num_rows * settings.tile_height) as i32;

                        s_output.set("width", out_width);
                        s_output.set("height", out_height);
                        gst::debug!(
                            CAT,
                            imp: self,
                            "setting tile_width:{} tile_height: {} out_width: {} out_height: {}",
                            settings.tile_width,
                            settings.tile_height,
                            out_width,
                            out_height
                        );
                    } else {
                        s_output.set("width", gst::IntRange::<i32>::new(1, u16::MAX as i32));
                        s_output.set("height", gst::IntRange::<i32>::new(1, u16::MAX as i32));
                    }
                    output_caps.append_structure(s_output);
                }
                output_caps.append(caps.clone());
            }

            output_caps
        };

        gst::debug!(
            CAT,
            imp: self,
            "Transformed caps from {} to {} in direction {:?}",
            caps,
            other_caps,
            direction
        );

        if let Some(filter) = filter {
            Some(filter.intersect_with_mode(&other_caps, gst::CapsIntersectMode::First))
        } else {
            Some(other_caps)
        }
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let in_info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse input caps"))?;

        let out_info = gst_video::VideoInfo::from_caps(outcaps)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to parse output caps"))?;

        gst::debug!(
            CAT,
            imp: self,
            "Configured for caps {} to {}",
            incaps,
            outcaps
        );

        let mut state = self.state.lock().unwrap();
        let old_out_info: Option<VideoInfo>;

        match *state {
            State::Started(ref mut state) => {
                old_out_info = state.out_info.take();

                state.in_info = Some(in_info);
                state.out_info = Some(out_info.clone());
            }
            State::Stopped => {
                unreachable!("set caps called in stopped state")
            }
        }
        drop(state);

        // in_info change does not matter, converter will be able to handle it
        // if output info change, Need to drain the current buffered canvas
        if old_out_info.is_some() && old_out_info != Some(out_info) {
            self.drain().map_err(|e| {
                gst::loggable_error!(CAT, "Error flushing canvas on caps change. {:?} ", e)
            })?;
        }

        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        *state = State::Started(Default::default());

        gst::info!(CAT, imp: self, "plugin Started");

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        *state = State::Stopped;
        gst::info!(CAT, imp: self, "plugin Stopped");

        Ok(())
    }
}

fn get_tile_width_height(
    tile_width: u32,
    tile_height: u32,
    in_height: i32,
    in_width: i32,
) -> (u32, u32) {
    let mut out_tile_height = tile_height;
    let mut out_tile_width = tile_width;

    if tile_width != 0 && tile_height == 0 {
        out_tile_height = (tile_width * in_height as u32) / in_width as u32;
    } else if tile_height != 0 && tile_width == 0 {
        out_tile_width = (tile_height * in_width as u32) / in_height as u32;
    } else if tile_width == 0 && tile_height == 0 {
        out_tile_width = in_width as u32;
        out_tile_height = in_height as u32;
    }

    return (out_tile_width, out_tile_height);
}
