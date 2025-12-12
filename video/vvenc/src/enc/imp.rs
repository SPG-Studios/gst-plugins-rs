// Copyright (C) 2025 Carlos Bentzen <cadubentzen@igalia.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

/**
 * SECTION:element-vvenc
 *
 * #vvenc is an encoder element for VVC/H.266 video streams using the VVenC encoder.
 *
 * ## Example pipeline
 *
 * Single-pass:
 * |[
 * gst-launch-1.0 videotestsrc num-buffers=10 ! vvenc ! h266parse ! isofmp4mux ! filesink location=vvc.mp4
 * ]|
 *
 * Two-pass:
 * |[
 * gst-launch-1.0 filesrc location=input.y4m ! y4mdec ! videoconvert ! vvenc num-passes=2 current-pass=first target-bitrate=200000 ! fakesink
 * gst-launch-1.0 filesrc location=input.y4m ! y4mdec ! videoconvert ! vvenc num-passes=2 current-pass=second target-bitrate=200000 ! h266parse ! isofmp4mux ! filesink location=vvc.mp4
 * ]|
 *
 *
 * Since: plugins-rs-0.14.0
 */
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;

use atomic_refcell::AtomicRefCell;
use gst::glib::{self, Properties};
use gst::subclass::prelude::*;
use gst_video::prelude::*;
use gst_video::subclass::prelude::*;
use std::sync::LazyLock;

use super::{CurrentPass, DecodingRefreshType, HdrMode, Level, Profile, SpeedPreset, Tier};

const DEFAULT_QP: i32 = 32;
const DEFAULT_THREADS: i32 = -1;
const DEFAULT_TARGET_BITRATE: i32 = 0;
const DEFAULT_INTRA_PERIOD: i32 = 0;
const DEFAULT_GOP_SIZE: i32 = 32;
const DEFAULT_N_PASSES: i32 = 1;
const DEFAULT_CURRENT_PASS: CurrentPass = CurrentPass::Single;
const DEFAULT_USE_PERCEPT_QPA: bool = false;
const DEFAULT_N_TILE_COLUMNS: i32 = -1;
const DEFAULT_N_TILE_ROWS: i32 = -1;
static DEFAULT_STATS_FILE: LazyLock<PathBuf> =
    LazyLock::new(|| PathBuf::from_str("vvenc.log").expect("valid path"));

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "vvenc",
        gst::DebugColorFlags::empty(),
        Some("VVenC VVC/H.266 encoder"),
    )
});

type SystemFrameNumber = u32;

struct State {
    encoder: vvenc::Encoder<SystemFrameNumber>,
    config: vvenc::Config,
    video_info: gst_video::VideoInfo,
    enc_buffer: vvenc::YUVBuffer<SystemFrameNumber>,
    output_data: Vec<u8>,
}

impl From<SpeedPreset> for vvenc::Preset {
    fn from(preset: SpeedPreset) -> Self {
        match preset {
            SpeedPreset::Faster => vvenc::Preset::Faster,
            SpeedPreset::Fast => vvenc::Preset::Fast,
            SpeedPreset::Medium => vvenc::Preset::Medium,
            SpeedPreset::Slow => vvenc::Preset::Slow,
            SpeedPreset::Slower => vvenc::Preset::Slower,
            SpeedPreset::MediumLowDecNrg => vvenc::Preset::MediumLowDecNrg,
            SpeedPreset::FirstPass => vvenc::Preset::FirstPass,
            SpeedPreset::ToolTest => vvenc::Preset::ToolTest,
        }
    }
}

impl From<DecodingRefreshType> for vvenc::DecodingRefreshType {
    fn from(drt: DecodingRefreshType) -> Self {
        match drt {
            DecodingRefreshType::None => vvenc::DecodingRefreshType::None,
            DecodingRefreshType::Cra => vvenc::DecodingRefreshType::Cra,
            DecodingRefreshType::Idr => vvenc::DecodingRefreshType::Idr,
            DecodingRefreshType::RecoveryPointSei => vvenc::DecodingRefreshType::RecoveryPointSei,
            DecodingRefreshType::CraCre => vvenc::DecodingRefreshType::CraCre,
            DecodingRefreshType::IdrNoRadl => vvenc::DecodingRefreshType::IdrNoRadl,
        }
    }
}

impl From<HdrMode> for vvenc::HdrMode {
    fn from(hdr_mode: HdrMode) -> Self {
        match hdr_mode {
            HdrMode::Off => vvenc::HdrMode::Off,
            HdrMode::Pq => vvenc::HdrMode::Pq,
            HdrMode::Hlg => vvenc::HdrMode::Hlg,
            HdrMode::PqBt2020 => vvenc::HdrMode::PqBt2020,
            HdrMode::HlgBt2020 => vvenc::HdrMode::HlgBt2020,
            HdrMode::UserDefined => vvenc::HdrMode::UserDefined,
            HdrMode::SdrBt709 => vvenc::HdrMode::SdrBt709,
            HdrMode::SdrBt2020 => vvenc::HdrMode::SdrBt2020,
            HdrMode::SdrBt470bg => vvenc::HdrMode::SdrBt470bg,
        }
    }
}

impl From<Level> for vvenc::Level {
    fn from(level: Level) -> Self {
        match level {
            Level::Auto => vvenc::Level::Auto,
            Level::Level1 => vvenc::Level::Level1,
            Level::Level2 => vvenc::Level::Level2,
            Level::Level2_1 => vvenc::Level::Level2_1,
            Level::Level3 => vvenc::Level::Level3,
            Level::Level3_1 => vvenc::Level::Level3_1,
            Level::Level4 => vvenc::Level::Level4,
            Level::Level4_1 => vvenc::Level::Level4_1,
            Level::Level5 => vvenc::Level::Level5,
            Level::Level5_1 => vvenc::Level::Level5_1,
            Level::Level5_2 => vvenc::Level::Level5_2,
            Level::Level6 => vvenc::Level::Level6,
            Level::Level6_1 => vvenc::Level::Level6_1,
            Level::Level6_2 => vvenc::Level::Level6_2,
            Level::Level6_3 => vvenc::Level::Level6_3,
            Level::Level15_5 => vvenc::Level::Level15_5,
        }
    }
}

impl From<Profile> for vvenc::Profile {
    fn from(profile: Profile) -> Self {
        match profile {
            Profile::Auto => vvenc::Profile::Auto,
            Profile::Main10 => vvenc::Profile::Main10,
            Profile::Main10StillPicture => vvenc::Profile::Main10StillPicture,
            Profile::Main10444 => vvenc::Profile::Main10444,
            Profile::Main10444StillPicture => vvenc::Profile::Main10444StillPicture,
            Profile::MultilayerMain10 => vvenc::Profile::MultilayerMain10,
            Profile::MultilayerMain10StillPicture => vvenc::Profile::MultilayerMain10StillPicture,
            Profile::MultilayerMain10444 => vvenc::Profile::MultilayerMain10444,
            Profile::MultilayerMain10444StillPicture => {
                vvenc::Profile::MultilayerMain10444StillPicture
            }
        }
    }
}

impl From<Tier> for vvenc::Tier {
    fn from(tier: Tier) -> Self {
        match tier {
            Tier::Main => vvenc::Tier::Main,
            Tier::High => vvenc::Tier::High,
        }
    }
}

fn video_format_to_chroma_format(format: gst_video::VideoFormat) -> vvenc::ChromaFormat {
    match format {
        gst_video::VideoFormat::Gray8 | gst_video::VideoFormat::Gray10Le16 => {
            vvenc::ChromaFormat::Chroma400
        }
        gst_video::VideoFormat::I420 | gst_video::VideoFormat::I42010le => {
            vvenc::ChromaFormat::Chroma420
        }
        gst_video::VideoFormat::Y42b | gst_video::VideoFormat::I42210le => {
            vvenc::ChromaFormat::Chroma422
        }
        gst_video::VideoFormat::Y444 | gst_video::VideoFormat::Y44410le => {
            vvenc::ChromaFormat::Chroma444
        }
        _ => unreachable!("video format enforced in caps"),
    }
}

#[derive(Debug)]
struct Settings {
    qp: i32,
    speed_preset: SpeedPreset,
    threads: i32,
    target_bitrate: i32,
    profile: Profile,
    tier: Tier,
    level: Level,
    intra_period: i32,
    decoding_refresh_type: DecodingRefreshType,
    gop_size: i32,
    n_passes: i32,
    current_pass: CurrentPass,
    stats_file: PathBuf,
    hdr_mode: HdrMode,
    use_percept_qpa: bool,
    n_tile_columns: i32,
    n_tile_rows: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            qp: DEFAULT_QP,
            speed_preset: SpeedPreset::default(),
            threads: DEFAULT_THREADS,
            target_bitrate: DEFAULT_TARGET_BITRATE,
            profile: Profile::default(),
            tier: Tier::default(),
            level: Level::default(),
            intra_period: DEFAULT_INTRA_PERIOD,
            decoding_refresh_type: DecodingRefreshType::default(),
            gop_size: DEFAULT_GOP_SIZE,
            n_passes: DEFAULT_N_PASSES,
            current_pass: DEFAULT_CURRENT_PASS,
            stats_file: DEFAULT_STATS_FILE.clone(),
            hdr_mode: HdrMode::default(),
            use_percept_qpa: DEFAULT_USE_PERCEPT_QPA,
            n_tile_columns: DEFAULT_N_TILE_COLUMNS,
            n_tile_rows: DEFAULT_N_TILE_ROWS,
        }
    }
}

#[derive(Default, Properties)]
#[properties(wrapper_type = super::VVenC)]
pub struct VVenC {
    state: AtomicRefCell<Option<State>>,
    #[property(name = "qp", get, set, type = i32, member = qp, minimum = 0, maximum = 63, default = DEFAULT_QP, blurb = "Quantization parameter")]
    #[property(name = "speed-preset", get, set, member = speed_preset, type = SpeedPreset, blurb = "Preset", builder(SpeedPreset::default()))]
    #[property(name = "threads", get, set, type = i32, member = threads, minimum=-1, default = DEFAULT_THREADS, blurb = "Number of threads (-1 for auto, limited by the number of available cores)")]
    #[property(name = "target-bitrate", get, set, type = i32, member = target_bitrate, minimum = 0, default = DEFAULT_TARGET_BITRATE, blurb = "Target bitrate in bps (0 = Rate Control disabled)")]
    #[property(name = "profile", get, set, type = Profile, member = profile, blurb = "Profile", builder(Profile::default()))]
    #[property(name = "tier", get, set, type = Tier, member = tier, blurb = "Tier", builder(Tier::default()))]
    #[property(name = "level", get, set, type = Level, member = level, blurb = "Level", builder(Level::default()))]
    #[property(name = "intra-period", get, set, type = i32, member = intra_period, minimum = 0, default = DEFAULT_INTRA_PERIOD, blurb = "Intra period in frames")]
    #[property(name = "decoding-refresh-type", get, set, type = DecodingRefreshType, member = decoding_refresh_type, blurb = "Decoding refresh type", builder(DecodingRefreshType::default()))]
    #[property(name = "gop-size", get, set, type = i32, member = gop_size, minimum = 0, default = DEFAULT_GOP_SIZE, blurb = "GOP size")]
    #[property(name = "n-passes", get, set, type = i32, member = n_passes, minimum=1, maximum=2, default = DEFAULT_N_PASSES, blurb = "Number of passes (only applicable if target-bitrate > 0)")]
    #[property(name = "current-pass", get, set, type = CurrentPass, member = current_pass, default = DEFAULT_CURRENT_PASS, blurb = "Current pass", builder(CurrentPass::default()))]
    #[property(name = "stats-file", get, set, type = PathBuf, member = stats_file, default = DEFAULT_STATS_FILE.as_os_str().to_str(), blurb = "Stats file for multipass encoding")]
    #[property(name = "hdr-mode", get, set, type = HdrMode, member = hdr_mode, blurb = "HDR mode", builder(HdrMode::default()))]
    #[property(name = "use-percept-qpa", get, set, type = bool, member = use_percept_qpa, default = DEFAULT_USE_PERCEPT_QPA, blurb = "Use perceptual QPA")]
    #[property(name = "n-tile-columns", get, set, type = i32, member = n_tile_columns, minimum = -1, default = DEFAULT_N_TILE_COLUMNS, blurb = "Number of tile columns")]
    #[property(name = "n-tile-rows", get, set, type = i32, member = n_tile_rows, minimum = -1, default = DEFAULT_N_TILE_ROWS, blurb = "Number of tile rows")]
    settings: Mutex<Settings>,
}

#[glib::object_subclass]
impl ObjectSubclass for VVenC {
    const NAME: &'static str = "GstVVenC";
    type Type = super::VVenC;
    type ParentType = gst_video::VideoEncoder;
}

#[glib::derived_properties]
impl ObjectImpl for VVenC {}

impl GstObjectImpl for VVenC {}

impl ElementImpl for VVenC {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "VVenC VVC/H.266 Encoder",
                "Codec/Encoder/Video",
                "Decode VVC/H.266 video streams with VVenC",
                "Carlos Bentzen <cadubentzen@igalia.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let sink_caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    gst_video::VideoFormat::I420,
                    gst_video::VideoFormat::I42010le,
                    gst_video::VideoFormat::Y42b,
                    gst_video::VideoFormat::I42210le,
                    gst_video::VideoFormat::Y444,
                    gst_video::VideoFormat::Y44410le,
                    gst_video::VideoFormat::Gray8,
                    gst_video::VideoFormat::Gray10Le16,
                ])
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &sink_caps,
            )
            .unwrap();

            let src_caps = gst::Caps::builder("video/x-h266")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

struct GstLogger;

impl vvenc::Logger for GstLogger {
    fn log(&self, level: vvenc::LogLevel, message: &str) {
        let level = GstLogger::gst_debug_level(level);
        let message = message.trim();
        gst::log_with_level!(CAT, level, "{message}");
    }
}

impl GstLogger {
    fn gst_debug_level(level: vvenc::LogLevel) -> gst::DebugLevel {
        match level {
            vvenc::LogLevel::Error => gst::DebugLevel::Error,
            vvenc::LogLevel::Warning => gst::DebugLevel::Warning,
            vvenc::LogLevel::Info => gst::DebugLevel::Info,
            vvenc::LogLevel::Notice => gst::DebugLevel::Debug,
            vvenc::LogLevel::Verbose => gst::DebugLevel::Log,
            vvenc::LogLevel::Details => gst::DebugLevel::Trace,
            _ => gst::DebugLevel::Info,
        }
    }
}

fn get_framerate(video_info: &gst_video::VideoInfo) -> vvenc::Rational {
    if video_info.fps() != gst::Fraction::new(0, 1) {
        vvenc::Rational {
            num: video_info.fps().numer() as i32,
            den: video_info.fps().denom() as i32,
        }
    } else {
        vvenc::Rational { num: 30, den: 1 }
    }
}

fn calculate_ticks_per_second(framerate: &vvenc::Rational) -> i32 {
    let num = framerate.num as f64;
    let den = framerate.den as f64;
    let ticks_per_second = (num / den).round() as i32 * 1000;
    ticks_per_second
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ticks_per_second() {
        let framerate = vvenc::Rational { num: 30, den: 1 };
        assert_eq!(calculate_ticks_per_second(&framerate), 30000);

        let framerate = vvenc::Rational {
            num: 30000,
            den: 1001,
        };
        assert_eq!(calculate_ticks_per_second(&framerate), 30000);

        let framerate = vvenc::Rational {
            num: 29970,
            den: 1000,
        };
        assert_eq!(calculate_ticks_per_second(&framerate), 30000);
    }
}

impl VideoEncoderImpl for VVenC {
    fn start(&self) -> Result<(), gst::ErrorMessage> {
        // Make sure we have the first DTS values non-negative. It's consistent with what other encoders do.
        self.obj()
            .set_min_pts(gst::ClockTime::from_seconds(60 * 60 * 1000));
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = None;
        Ok(())
    }

    fn propose_allocation(
        &self,
        query: &mut gst::query::Allocation,
    ) -> Result<(), gst::LoggableError> {
        query.add_allocation_meta::<gst_video::VideoMeta>(None);
        self.parent_propose_allocation(query)
    }

    fn set_format(
        &self,
        state: &gst_video::VideoCodecState<'static, gst_video::video_codec_state::Readable>,
    ) -> Result<(), gst::LoggableError> {
        self.finish()
            .map_err(|_| gst::loggable_error!(CAT, "Failed to drain"))?;

        let video_info = state.info();
        gst::debug!(CAT, imp = self, "Setting format {:?}", video_info);

        let settings = self.settings.lock().unwrap();

        let width = video_info.width() as i32;
        let height = video_info.height() as i32;
        let framerate = get_framerate(&video_info);
        let ticks_per_second = calculate_ticks_per_second(&framerate);

        let qp = vvenc::Qp::new(settings.qp as u8).expect("valid qp range enforced in properties");
        let chroma_format = video_format_to_chroma_format(video_info.format());
        let preset = vvenc::Preset::from(settings.speed_preset);
        let bit_depth = video_info.format_info().bits() as i32;

        let mut config = vvenc::Config::default();
        config
            .set_width(width)
            .set_height(height)
            .set_framerate(framerate)
            .set_qp(qp)
            .set_num_threads(settings.threads)
            .set_ticks_per_second(ticks_per_second)
            .set_target_bitrate(settings.target_bitrate)
            .set_profile(vvenc::Profile::from(settings.profile))
            .set_tier(vvenc::Tier::from(settings.tier))
            .set_level(vvenc::Level::from(settings.level))
            .set_intra_period(settings.intra_period)
            .set_decoding_refresh_type(vvenc::DecodingRefreshType::from(
                settings.decoding_refresh_type,
            ))
            .set_gop_size(settings.gop_size)
            .set_num_passes(settings.n_passes)
            .set_hdr_mode(vvenc::HdrMode::from(settings.hdr_mode))
            .set_use_percept_qpa(settings.use_percept_qpa)
            .set_num_tile_columns(settings.n_tile_columns)
            .set_num_tile_rows(settings.n_tile_rows)
            .set_input_bit_depth([bit_depth, 0])
            .set_output_bit_depth([bit_depth, 0])
            .set_internal_bit_depth([bit_depth, 0]);

        config
            .set_preset(preset)
            .map_err(|err| gst::loggable_error!(CAT, "Failed to set preset {preset:?}: {err:?}"))?;

        config
            .set_log_level(vvenc::LogLevel::Details)
            .set_logger(Box::new(GstLogger));

        // VVenC only supports 4:2:0 and 4:0:0 internal chroma formats. 4:2:0 is the default,
        // but if we have a 4:0:0 input, we need to set the internal format to 4:0:0 as well.
        if chroma_format == vvenc::ChromaFormat::Chroma400 {
            config.set_internal_chroma_format(chroma_format);
        }

        let mut encoder = vvenc::Encoder::with_config(config.clone())
            .map_err(|err| gst::loggable_error!(CAT, "Failed to create encoder: {:?}", err))?;

        if settings.target_bitrate > 0 && settings.n_passes > 1 {
            encoder
                .init_pass(settings.current_pass as i32, &settings.stats_file)
                .map_err(|err| {
                    gst::loggable_error!(
                        CAT,
                        "Failed to init pass {:?}: {:?}",
                        settings.current_pass,
                        err
                    )
                })?;
        }

        // Reserve size for the largest possible AU, like done in vvencapp.
        let au_size_scale = match chroma_format {
            vvenc::ChromaFormat::Chroma400 | vvenc::ChromaFormat::Chroma420 => 2,
            _ => 3,
        };
        let output_data = vec![0; (au_size_scale * width * height + 1024) as usize];
        let input_buffer = vvenc::YUVBuffer::new(width, height, chroma_format);
        *self.state.borrow_mut() = Some(State {
            encoder,
            config,
            video_info: video_info.clone(),
            enc_buffer: input_buffer,
            output_data,
        });

        let instance = self.obj();
        let output_state = instance
            .set_output_state(
                gst::Caps::builder("video/x-h266")
                    .field("stream-format", "byte-stream")
                    .field("alignment", "au")
                    .build(),
                Some(state),
            )
            .map_err(|_| gst::loggable_error!(CAT, "Failed to set output state"))?;
        instance
            .negotiate(output_state)
            .map_err(|_| gst::loggable_error!(CAT, "Failed to negotiate"))?;

        self.parent_set_format(state)
    }

    fn flush(&self) -> bool {
        gst::debug!(CAT, imp = self, "Flushing");

        let mut state_guard = self.state.borrow_mut();
        if let Some(ref mut state) = *state_guard {
            while let Ok(Some(_)) = state.encoder.flush(&mut state.output_data) {
                gst::debug!(CAT, imp = self, "Dropping packet on flush");
            }
        }

        true
    }

    fn finish(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::debug!(CAT, imp = self, "Finishing");

        let mut state_guard = self.state.borrow_mut();
        if let Some(ref mut state) = *state_guard {
            let ticks_per_second = state.config.ticks_per_second() as u64;
            loop {
                match state.encoder.flush(&mut state.output_data) {
                    Ok(Some((au, encode_done))) => {
                        gst::debug!(CAT, imp = self, "Flushed access unit");
                        self.handle_access_unit(au, ticks_per_second)?;
                        if encode_done {
                            gst::debug!(CAT, imp = self, "Finished encoding");
                            break;
                        }
                    }
                    Ok(None) => {
                        gst::debug!(CAT, imp = self, "No more access units to flush");
                        break;
                    }
                    Err(vvenc::Error::RestartRequired) => {
                        gst::debug!(CAT, imp = self, "Restart required during flush");
                        break;
                    }
                    Err(err) => {
                        gst::error!(CAT, imp = self, "Failed to flush: {:?}", err);
                        return Err(gst::FlowError::Error);
                    }
                }
            }
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn handle_frame(
        &self,
        frame: gst_video::VideoCodecFrame,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        let system_frame_number = frame.system_frame_number();
        gst::trace!(CAT, imp = self, "Handling frame {system_frame_number}");

        let input_buffer = frame.input_buffer().expect("frame without input buffer");

        let in_frame =
            gst_video::VideoFrameRef::from_buffer_ref_readable(input_buffer, &state.video_info)
                .map_err(|_| {
                    gst::element_imp_error!(
                        self,
                        gst::CoreError::Failed,
                        ["Failed to map output buffer readable"]
                    );
                    gst::FlowError::Error
                })?;

        let ticks_per_second = calculate_ticks_per_second(&get_framerate(&state.video_info)) as u64;
        // We need PTS to correctly carry opaque data with sfn in vvenc-rs until
        // https://github.com/fraunhoferhhi/vvenc/pull/513 gets into a stable VVenC release.
        let Some(pts) = frame.pts() else {
            gst::error!(
                CAT,
                imp = self,
                "We need frames to have PTS for correctly mapping to system frame numbers"
            );
            return Err(gst::FlowError::Error);
        };

        match state.encode_frame(&in_frame, system_frame_number, pts) {
            Ok(Some(au)) => self.handle_access_unit(au, ticks_per_second),
            Ok(None) => Ok(gst::FlowSuccess::Ok),
            Err(err) => {
                gst::error!(CAT, imp = self, "Failed to encode frame: {:?}", err);
                Err(gst::FlowError::Error)
            }
        }
    }
}

impl State {
    fn encode_frame(
        &mut self,
        in_frame: &gst_video::VideoFrameRef<&gst::BufferRef>,
        system_frame_number: SystemFrameNumber,
        pts: gst::ClockTime,
    ) -> Result<Option<vvenc::AccessUnit<SystemFrameNumber>>, vvenc::Error> {
        let components = if self.video_info.is_yuv() {
            vec![
                vvenc::YUVComponent::Y,
                vvenc::YUVComponent::U,
                vvenc::YUVComponent::V,
            ]
        } else if self.video_info.is_gray() {
            vec![vvenc::YUVComponent::Y]
        } else {
            unreachable!()
        };

        // If input has 2 bytes per sample, we can avoid copying altogether and just use the input buffer.
        let bytes_per_sample = in_frame.format_info().bits().div_ceil(8) as usize;
        if bytes_per_sample == 2 {
            let planes = components
                .iter()
                .copied()
                .map(|component| {
                    vvenc::Plane::from_slice(
                        bytemuck::cast_slice(in_frame.plane_data(component as u32).unwrap()),
                        in_frame.comp_width(component as u32) as i32,
                        in_frame.comp_height(component as u32) as i32,
                        in_frame.plane_stride()[component as usize] as i32
                            / bytes_per_sample as i32,
                    )
                })
                .collect::<Vec<_>>();
            self.enc_buffer = vvenc::YUVBuffer::<SystemFrameNumber>::from_planes(&planes);
        }
        // Else, we need to copy the data to the encoder input buffer, which although has 8-bit input depth,
        // it still is carried through an i16 slice.
        else if bytes_per_sample == 1 {
            assert!(self.enc_buffer.is_owned());
            for component in components {
                let mut out_plane = self.enc_buffer.plane_mut(component);
                let out_stride = out_plane.stride() as usize;
                let out_data = out_plane.data_mut();

                let in_data = in_frame.plane_data(component as u32).unwrap();
                let in_stride = in_frame.plane_stride()[component as usize] as usize;

                for (out_line, in_line) in out_data
                    .chunks_exact_mut(out_stride)
                    .zip(in_data.chunks_exact(in_stride))
                {
                    out_line
                        .iter_mut()
                        .zip(in_line.iter())
                        .for_each(|(out, in_)| {
                            *out = *in_ as i16;
                        });
                }
            }
        } else {
            unreachable!();
        }

        let ticks_per_second = self.config.ticks_per_second() as u64;
        let pts = (pts.useconds() * ticks_per_second) / 1_000_000;
        self.enc_buffer.set_cts(pts);
        self.enc_buffer.set_opaque(system_frame_number);
        self.encoder
            .encode(&mut self.enc_buffer, &mut self.output_data)
    }
}

impl VVenC {
    fn handle_access_unit(
        &self,
        mut au: vvenc::AccessUnit<SystemFrameNumber>,
        ticks_per_second: u64,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let frame_size = au.payload().len();
        let slice_type = au.slice_type();
        let sfn = *au
            .take_opaque()
            .expect("frame with opaque containing system frame number");

        gst::debug!(
            CAT,
            imp = self,
            "Received encoded frame {sfn} of size {frame_size}, slice type {slice_type:?}"
        );

        let instance = self.obj();
        let mut frame = instance.frame(sfn as i32).expect("frame not found");

        if slice_type == vvenc::SliceType::I {
            frame.set_flags(gst_video::VideoCodecFrameFlags::SYNC_POINT);
        }

        if let Some(pts) = au.cts() {
            let pts = (pts * 1_000_000) / ticks_per_second;
            let pts = gst::ClockTime::from_useconds(pts);
            gst::trace!(CAT, imp = self, "Access unit pts {:?}", pts);
            frame.set_pts(pts);
        }

        if let Some(dts) = au.dts() {
            let dts = (dts * 1_000_000) / ticks_per_second;
            let dts = gst::ClockTime::from_useconds(dts);
            gst::trace!(CAT, imp = self, "Access unit dts {:?}", dts);
            frame.set_dts(dts);
        }

        let mem = gst::Memory::with_size(frame_size);
        let mut writable_mem = mem
            .into_mapped_memory_writable()
            .map_err(|_| gst::FlowError::Error)?;
        writable_mem.as_mut_slice().copy_from_slice(au.payload());

        let mut output_buffer = gst::Buffer::new();
        let mut_buffer = output_buffer.get_mut().unwrap();
        mut_buffer.append_memory(writable_mem.into_memory());

        frame.set_output_buffer(output_buffer);
        instance.finish_frame(frame)
    }
}
