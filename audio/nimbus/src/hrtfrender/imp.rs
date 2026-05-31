use gst::glib;
use gst::glib::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_audio::subclass::prelude::*;

use std::sync::{LazyLock, Mutex};

const SAMPLE_RATE: u32 = 48000;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "audionimbushrtf",
        gst::DebugColorFlags::empty(),
        Some("AudioNimbusHrtf"),
    )
});

#[derive(Debug)]
struct Settings {
    block_length: u32,
    direction: audionimbus::Direction,
    sofa_file: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            block_length: 48,
            direction: audionimbus::Direction::new(0.0, 0.0, -1.0),
            sofa_file: None,
        }
    }
}

#[derive(Debug)]
struct State {
    in_info: gst_audio::AudioInfo,
    out_info: gst_audio::AudioInfo,
    adapter: gst_base::UniqueAdapter,
    processing: Option<Processing>,
    output_samples: u32,
}

#[derive(Debug)]
struct Processing {
    hrtf: audionimbus::Hrtf,
    context: audionimbus::Context,
    _settings: audionimbus::AudioSettings,
    binaural: audionimbus::BinauralEffect,
}

impl Default for State {
    fn default() -> Self {
        Self {
            in_info: gst_audio::AudioInfo::builder(gst_audio::AUDIO_FORMAT_F32, SAMPLE_RATE, 1)
                .build()
                .unwrap(),
            out_info: gst_audio::AudioInfo::builder(gst_audio::AUDIO_FORMAT_F32, SAMPLE_RATE, 2)
                .layout(gst_audio::AudioLayout::Interleaved)
                .build()
                .unwrap(),
            adapter: gst_base::UniqueAdapter::default(),
            processing: None,
            output_samples: 0,
        }
    }
}

impl State {
    fn input_block_size(&self) -> usize {
        (self.output_samples * self.in_info.bpf()) as usize
    }

    fn output_block_size(&self) -> usize {
        (self.output_samples * self.out_info.bpf()) as usize
    }
}

#[derive(Debug, Default)]
pub struct NimbusHrtf {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for NimbusHrtf {
    const NAME: &'static str = "GstAudioNimbusHrtfRender";
    type Type = super::NimbusHrtf;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for NimbusHrtf {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("sofa")
                .nick("Sofa")
                .blurb("The Sofa file to use for rendering. If not set, a default SOFA file is used.")
                .build(),
                glib::ParamSpecUInt::builder("block-length")
                .nick("Block Length")
                .blurb("Number of samples to process at a time.")
                .minimum(1)
                .maximum(u32::MAX)
                .default_value(256)
                .build(),
                glib::ParamSpecFloat::builder("pos-x")
                .nick("X Position")
                .blurb("The Cartesian X coordinate of the sound source in relation to the listener at 0, 0, 0. Negative values are towards the left.")
                .minimum(-1.0)
                .maximum(1.0)
                .default_value(0.0)
                .build(),
                glib::ParamSpecFloat::builder("pos-y")
                .nick("Y Position")
                .blurb("The Cartesian Y coordinate of the sound source in relation to the listener at 0, 0, 0. Negative values are below the listener.")
                .minimum(-1.0)
                .maximum(1.0)
                .default_value(0.0)
                .build(),
                glib::ParamSpecFloat::builder("pos-z")
                .nick("Z Position")
                .blurb("The Cartesian Z coordinate of the sound source in relation to the listener at 0, 0, 0. Negative values are in front of the listener.")
                .minimum(-1.0)
                .maximum(1.0)
                .default_value(-1.0)
                .build(),
                ]
        });
        PROPS.as_ref()
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "sofa" => self.settings.lock().unwrap().sofa_file.to_value(),
            "block-length" => self.settings.lock().unwrap().block_length.to_value(),
            "pos-x" => self.settings.lock().unwrap().direction.x.to_value(),
            "pos-y" => self.settings.lock().unwrap().direction.y.to_value(),
            "pos-z" => self.settings.lock().unwrap().direction.z.to_value(),
            _ => unreachable!(),
        }
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "sofa" => self.settings.lock().unwrap().sofa_file = value.get().unwrap(),
            "block-length" => self.settings.lock().unwrap().block_length = value.get().unwrap(),
            "pos-x" => self.settings.lock().unwrap().direction.x = value.get().unwrap(),
            "pos-y" => self.settings.lock().unwrap().direction.y = value.get().unwrap(),
            "pos-z" => self.settings.lock().unwrap().direction.z = value.get().unwrap(),
            _ => unreachable!(),
        }
    }
}

impl GstObjectImpl for NimbusHrtf {}

impl ElementImpl for NimbusHrtf {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Nimbus HRTF Renderer",
                "Filter/Effect/Audio",
                "Renders audio using a Head Related Transfer Function",
                "Matthew Waters <matthew@centricular.com>",
            )
        });
        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let audio_input_caps = gst_audio::audio_make_raw_caps(
                &[gst_audio::AUDIO_FORMAT_F32],
                gst_audio::AudioLayout::Interleaved,
            )
            .rate(SAMPLE_RATE as i32)
            .channels(1)
            .build();

            let audio_output_caps = gst_audio::audio_make_raw_caps(
                &[gst_audio::AUDIO_FORMAT_F32],
                gst_audio::AudioLayout::Interleaved,
            )
            .rate(SAMPLE_RATE as i32)
            .channels(2)
            .channel_mask(0x3)
            .build();

            vec![
                gst::PadTemplate::builder(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &audio_output_caps,
                )
                .build()
                .unwrap(),
                gst::PadTemplate::builder(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &audio_input_caps,
                )
                .build()
                .unwrap(),
            ]
        });

        &PAD_TEMPLATES
    }
}

impl BaseTransformImpl for NimbusHrtf {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::NeverInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = self.settings.lock().unwrap();
        let output_samples = settings.block_length;

        let context = audionimbus::Context::default();
        let audio_settings = audionimbus::AudioSettings {
            sampling_rate: SAMPLE_RATE,
            frame_size: output_samples,
        };
        let mut hrtf_settings = audionimbus::HrtfSettings::default();
        if let Some(sofa) = settings.sofa_file.as_ref() {
            hrtf_settings.sofa_information = Some(audionimbus::Sofa::Filename(sofa.clone()));
        }
        let hrtf = audionimbus::Hrtf::try_new(&context, &audio_settings, &hrtf_settings).unwrap();
        let effect_settings = audionimbus::BinauralEffectSettings { hrtf: hrtf.clone() };
        let effect =
            audionimbus::BinauralEffect::try_new(&context, &audio_settings, &effect_settings)
                .unwrap();
        let mut state = self.state.lock().unwrap();
        state.processing = Some(Processing {
            hrtf,
            context,
            _settings: audio_settings,
            binaural: effect,
        });
        state.output_samples = output_samples;
        Ok(())
    }

    fn transform_caps(
        &self,
        direction: gst::PadDirection,
        _caps: &gst::Caps,
        _filter: Option<&gst::Caps>,
    ) -> Option<gst::Caps> {
        let templ_caps = match direction {
            gst::PadDirection::Src => self.obj().pad_template("sink").unwrap().caps().clone(),
            gst::PadDirection::Sink => self.obj().pad_template("src").unwrap().caps().clone(),
            _ => unreachable!(),
        };
        Some(templ_caps)
    }
    /*
    fn negotiated_src_caps(&self, caps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let Ok(audio_info) = gst_audio::AudioInfo::from_caps(caps) else {
            return Err(gst::loggable_error!(CAT, "Failed to parse src caps"));
        };
        let mut state = self.state.lock().unwrap();
        state.out_info = audio_info;

        self.parent_negotiated_src_caps(caps)
    }
    */
    fn transform(
        &self,
        inbuf: &gst::Buffer,
        outbuf: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let direction = self.settings.lock().unwrap().direction;
        let mut state = self.state.lock().unwrap();

        state.adapter.push(inbuf.clone());
        let num_frames = state.output_samples as usize;
        if state.processing.is_none() {
            return Err(gst::FlowError::NotNegotiated);
        };

        let in_block_size = state.input_block_size();
        let out_block_size = state.output_block_size();
        let Ok(mut output) = outbuf.map_writable() else {
            return Err(gst::FlowError::Error);
        };
        let output_f32 = unsafe {
            let (prefix, middle, suffix) = output.align_to_mut::<f32>();
            assert!(prefix.is_empty());
            assert!(suffix.is_empty());
            middle
        };
        debug_assert_eq!(output_f32.len() % (out_block_size / size_of::<f32>()), 0);
        let mut written = 0;
        while state.adapter.available() >= in_block_size {
            let inbuf = state.adapter.take_buffer(in_block_size).unwrap();
            let Ok(input) = inbuf.map_readable() else {
                return Err(gst::FlowError::Error);
            };
            let input = unsafe {
                let (prefix, middle, suffix) = input.align_to::<f32>();
                assert!(prefix.is_empty());
                assert!(suffix.is_empty());
                middle
            };
            assert_eq!(input.len(), num_frames);
            let input = audionimbus::AudioBuffer::try_with_data(input).unwrap();
            let mut scratch = vec![0f32; num_frames * 2];
            let output_buf = audionimbus::AudioBuffer::try_with_data_and_settings(
                &mut scratch,
                audionimbus::AudioBufferSettings::with_num_channels(2),
            )
            .unwrap();
            let processing = state.processing.as_mut().unwrap();
            let params = audionimbus::BinauralEffectParams {
                direction,
                interpolation: audionimbus::HrtfInterpolation::Bilinear,
                spatial_blend: 1.0,
                hrtf: processing.hrtf.clone(),
                peak_delays: None,
            };
            processing
                .binaural
                .apply(&params, &input, &output_buf)
                .unwrap();

            output_buf
                .interleave(
                    &processing.context,
                    &mut output_f32[written * 2..][..num_frames * 2],
                )
                .unwrap();
            written += num_frames;
        }
        drop(output);
        /*
        gst_audio::AudioMeta::add(
            outbuf,
            &state.out_info,
            written,
            &[0, written * size_of::<f32>()],
        )
        .unwrap();*/
        Ok(gst::FlowSuccess::Ok)
    }

    fn transform_size(
        &self,
        _direction: gst::PadDirection,
        _caps: &gst::Caps,
        size: usize,
        _othercaps: &gst::Caps,
    ) -> Option<usize> {
        assert_ne!(_direction, gst::PadDirection::Src);

        let state = self.state.lock().unwrap();

        let othersize = {
            let full_blocks = (size + state.adapter.available()) / (state.input_block_size());
            full_blocks * state.output_block_size()
        };

        gst::log!(
            CAT,
            imp = self,
            "Adapter size: {}, input size {}, transformed size {}",
            state.adapter.available(),
            size,
            othersize,
        );

        Some(othersize)
    }
}
