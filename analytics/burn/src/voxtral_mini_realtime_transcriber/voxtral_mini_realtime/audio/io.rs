//! Audio I/O utilities.
//!
//! Handles loading and saving WAV files with automatic format conversion.

use anyhow::{Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use std::path::Path;

/// Audio buffer with samples and metadata.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    /// Audio samples (mono, normalized to [-1.0, 1.0])
    pub samples: Vec<f32>,
    /// Sample rate in Hz
    pub sample_rate: u32,
}

impl AudioBuffer {
    /// Create a new audio buffer.
    pub fn new(samples: Vec<f32>, sample_rate: u32) -> Self {
        Self {
            samples,
            sample_rate,
        }
    }

    /// Create an empty buffer at the given sample rate.
    pub fn empty(sample_rate: u32) -> Self {
        Self {
            samples: Vec::new(),
            sample_rate,
        }
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Duration in seconds.
    pub fn duration_secs(&self) -> f32 {
        self.samples.len() as f32 / self.sample_rate as f32
    }

    /// Duration in milliseconds.
    pub fn duration_ms(&self) -> f32 {
        self.duration_secs() * 1000.0
    }

    /// Normalize audio to target peak amplitude.
    ///
    /// Scales all samples so the maximum absolute value equals `target_peak`.
    /// Returns self unchanged if audio is silent (max amplitude < 1e-10).
    pub fn peak_normalize(&mut self, target_peak: f32) {
        let max_amp = self.samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        if max_amp < 1e-10 {
            return;
        }
        let scale = target_peak / max_amp;
        for s in &mut self.samples {
            *s *= scale;
        }
    }

    /// Append samples from another buffer (must have same sample rate).
    pub fn append(&mut self, other: &AudioBuffer) -> Result<()> {
        if self.sample_rate != other.sample_rate {
            anyhow::bail!(
                "Sample rate mismatch: {} vs {}",
                self.sample_rate,
                other.sample_rate
            );
        }
        self.samples.extend_from_slice(&other.samples);
        Ok(())
    }

    /// Save to WAV file.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        save_wav(self, path)
    }
}

/// Load a WAV file and return as mono f32 samples.
pub fn load_wav<P: AsRef<Path>>(path: P) -> Result<AudioBuffer> {
    let path = path.as_ref();
    let reader = WavReader::open(path)
        .with_context(|| format!("Failed to open WAV file: {}", path.display()))?;

    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as usize;

    let samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Int => {
            let bits = spec.bits_per_sample;
            let max_val = (1i32 << (bits - 1)) as f32;

            reader
                .into_samples::<i32>()
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to read WAV samples")?
                .chunks(channels)
                .map(|chunk| {
                    // Mix to mono by averaging channels
                    let sum: i32 = chunk.iter().sum();
                    (sum as f32 / channels as f32) / max_val
                })
                .collect()
        }
        SampleFormat::Float => {
            reader
                .into_samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .context("Failed to read WAV samples")?
                .chunks(channels)
                .map(|chunk| {
                    // Mix to mono by averaging channels
                    chunk.iter().sum::<f32>() / channels as f32
                })
                .collect()
        }
    };

    Ok(AudioBuffer::new(samples, sample_rate))
}

/// Save audio buffer to WAV file (16-bit PCM).
pub fn save_wav<P: AsRef<Path>>(audio: &AudioBuffer, path: P) -> Result<()> {
    let path = path.as_ref();
    let spec = WavSpec {
        channels: 1,
        sample_rate: audio.sample_rate,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut writer = WavWriter::create(path, spec)
        .with_context(|| format!("Failed to create WAV file: {}", path.display()))?;

    for &sample in &audio.samples {
        // Clamp and convert to i16
        let clamped = sample.clamp(-1.0, 1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        writer.write_sample(i16_sample)?;
    }

    writer.finalize()?;
    Ok(())
}
