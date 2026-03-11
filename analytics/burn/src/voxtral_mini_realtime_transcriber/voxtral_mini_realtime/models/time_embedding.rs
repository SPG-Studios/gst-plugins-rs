//! Time embedding for Voxtral Realtime.
//!
//! Sinusoidal embedding that encodes the transcription delay.

use burn::prelude::*;

/// Time embedding module that produces sinusoidal embeddings.
///
/// Used to encode the transcription delay as a conditioning signal
/// for the ADA RMSNorm modulation in the decoder layers.
#[derive(Debug)]
pub struct TimeEmbedding {
    /// Dimension of the embedding
    dim: usize,
    /// Base frequency (default: 10000.0)
    theta: f32,
}

impl TimeEmbedding {
    /// Create a new time embedding with given dimension.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            theta: 10000.0,
        }
    }

    /// Create a new time embedding with custom theta.
    pub fn with_theta(dim: usize, theta: f32) -> Self {
        Self { dim, theta }
    }

    /// Compute sinusoidal embedding for a time value.
    ///
    /// # Arguments
    /// * `t` - Time value (typically the number of delay tokens)
    /// * `device` - Device to create the tensor on
    ///
    /// # Returns
    /// Tensor of shape [1, 1, dim] containing the sinusoidal embedding
    pub fn embed<B: Backend>(&self, t: f32, device: &B::Device) -> Tensor<B, 3> {
        let half_dim = self.dim / 2;

        // Compute inverse frequencies: exp(-log(theta) * i / (dim/2)) for i in 0..dim/2
        let mut inv_freq = Vec::with_capacity(half_dim);
        let log_theta = self.theta.ln();
        for i in 0..half_dim {
            let freq = (-log_theta * (i as f32) / (half_dim as f32)).exp();
            inv_freq.push(freq);
        }

        // Compute t * inv_freq
        let mut cos_vals = Vec::with_capacity(half_dim);
        let mut sin_vals = Vec::with_capacity(half_dim);
        for &freq in &inv_freq {
            let angle = t * freq;
            cos_vals.push(angle.cos());
            sin_vals.push(angle.sin());
        }

        // Concatenate [cos, sin] to get full embedding
        let mut embedding = Vec::with_capacity(self.dim);
        embedding.extend_from_slice(&cos_vals);
        embedding.extend_from_slice(&sin_vals);

        // Create tensor with shape [1, 1, dim]
        Tensor::from_data(
            burn::tensor::TensorData::new(embedding, [1, 1, self.dim]),
            device,
        )
    }
}
