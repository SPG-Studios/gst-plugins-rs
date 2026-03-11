//! Multi-head attention with RoPE and causal masking.
//!
//! Supports both MHA (encoder) and GQA (LLM) configurations.

use burn::config::Config;
use burn::module::Module;
use burn::nn::{Linear, LinearConfig};
use burn::tensor::activation::softmax;
use burn::tensor::backend::Backend;
use burn::tensor::{Float, Tensor};

use super::kv_cache::KVCache;
use super::rope::RoPE;

/// Attention configuration.
#[derive(Config, Debug)]
pub struct AttentionConfig {
    /// Model dimension.
    pub d_model: usize,
    /// Number of query heads.
    pub n_heads: usize,
    /// Number of KV heads (for GQA). If None, uses n_heads (MHA).
    pub n_kv_heads: Option<usize>,
    /// Head dimension (usually d_model / n_heads).
    pub head_dim: usize,
    /// Whether to use bias on Q projection.
    #[config(default = false)]
    pub q_bias: bool,
    /// Whether to use bias on K projection.
    #[config(default = false)]
    pub k_bias: bool,
    /// Whether to use bias on V projection.
    #[config(default = false)]
    pub v_bias: bool,
    /// Whether to use bias on O projection.
    #[config(default = false)]
    pub o_bias: bool,
    /// Sliding window size (None for full attention).
    pub sliding_window: Option<usize>,
}

/// Multi-head attention layer.
#[derive(Module, Debug)]
pub struct Attention<B: Backend> {
    wq: Linear<B>,
    wk: Linear<B>,
    wv: Linear<B>,
    wo: Linear<B>,
    n_heads: usize,
    n_kv_heads: usize,
    head_dim: usize,
    scale: f32,
    sliding_window: Option<usize>,
}

impl AttentionConfig {
    /// Initialize the attention layer.
    pub fn init<B: Backend>(&self, device: &B::Device) -> Attention<B> {
        let n_kv_heads = self.n_kv_heads.unwrap_or(self.n_heads);

        let wq = LinearConfig::new(self.d_model, self.n_heads * self.head_dim)
            .with_bias(self.q_bias)
            .init(device);
        let wk = LinearConfig::new(self.d_model, n_kv_heads * self.head_dim)
            .with_bias(self.k_bias)
            .init(device);
        let wv = LinearConfig::new(self.d_model, n_kv_heads * self.head_dim)
            .with_bias(self.v_bias)
            .init(device);
        let wo = LinearConfig::new(self.n_heads * self.head_dim, self.d_model)
            .with_bias(self.o_bias)
            .init(device);

        Attention {
            wq,
            wk,
            wv,
            wo,
            n_heads: self.n_heads,
            n_kv_heads,
            head_dim: self.head_dim,
            scale: (self.head_dim as f32).powf(-0.5),
            sliding_window: self.sliding_window,
        }
    }
}

impl<B: Backend> Attention<B> {
    /// Create attention from linear layers (for weight loading).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        wq: Linear<B>,
        wk: Linear<B>,
        wv: Linear<B>,
        wo: Linear<B>,
        n_heads: usize,
        n_kv_heads: usize,
        head_dim: usize,
        sliding_window: Option<usize>,
    ) -> Self {
        Self {
            wq,
            wk,
            wv,
            wo,
            n_heads,
            n_kv_heads,
            head_dim,
            scale: (head_dim as f32).powf(-0.5),
            sliding_window,
        }
    }

    /// Forward pass with RoPE.
    ///
    /// # Arguments
    /// * `x` - Input tensor [batch, seq, d_model]
    /// * `rope` - Rotary position embeddings
    /// * `offset` - Position offset for KV cache
    /// * `causal` - Whether to apply causal masking
    ///
    /// # Returns
    /// Output tensor [batch, seq, d_model]
    pub fn forward(
        &self,
        x: Tensor<B, 3>,
        rope: &RoPE<B>,
        offset: usize,
        causal: bool,
    ) -> Tensor<B, 3> {
        let [batch, seq_len, _d_model] = x.dims();

        // Project Q, K, V
        let q = self.wq.forward(x.clone());
        let k = self.wk.forward(x.clone());
        let v = self.wv.forward(x);

        // Reshape to [batch, seq, heads, head_dim]
        let q = q.reshape([batch, seq_len, self.n_heads, self.head_dim]);
        let k = k.reshape([batch, seq_len, self.n_kv_heads, self.head_dim]);
        let v = v.reshape([batch, seq_len, self.n_kv_heads, self.head_dim]);

        // Apply RoPE
        let (q, k) = rope.apply(q, k, offset);

        // Transpose to [batch, heads, seq, head_dim]
        let q = q.swap_dims(1, 2);
        let k = k.swap_dims(1, 2);
        let v = v.swap_dims(1, 2);

        // Expand K, V for GQA if needed
        let (k, v) = self.expand_kv(k, v);

        // Compute attention scores: Q @ K^T * scale
        let k_t = k.swap_dims(2, 3);
        let scores = q.matmul(k_t) * self.scale;

        // Apply causal mask
        let scores = if causal {
            self.apply_causal_mask(scores, seq_len, offset)
        } else {
            scores
        };

        // Apply sliding window mask if configured
        let scores = if let Some(window) = self.sliding_window {
            self.apply_sliding_window_mask(scores, seq_len, window)
        } else {
            scores
        };

        // Softmax
        let attn = softmax(scores, 3);

        // Apply attention: attn @ V
        let out = attn.matmul(v);

        // Transpose back and reshape: [batch, heads, seq, head_dim] -> [batch, seq, heads * head_dim]
        let out = out.swap_dims(1, 2);
        let out = out.reshape([batch, seq_len, self.n_heads * self.head_dim]);

        // Output projection
        self.wo.forward(out)
    }

    /// Forward pass with KV cache.
    ///
    /// # Arguments
    /// * `x` - Input tensor [batch, seq, d_model]
    /// * `rope` - Rotary position embeddings
    /// * `cache` - Mutable KV cache (updated in place)
    /// * `causal` - Whether to apply causal masking
    ///
    /// # Returns
    /// Output tensor [batch, seq, d_model]
    pub fn forward_with_cache(
        &self,
        x: Tensor<B, 3>,
        rope: &RoPE<B>,
        cache: &mut KVCache<B>,
        causal: bool,
    ) -> Tensor<B, 3> {
        let [batch, seq_len, _d_model] = x.dims();

        // Position offset is the current cache length
        let offset = cache.seq_len();

        // Project Q, K, V
        let q = self.wq.forward(x.clone());
        let k = self.wk.forward(x.clone());
        let v = self.wv.forward(x);

        // Reshape to [batch, seq, heads, head_dim]
        let q = q.reshape([batch, seq_len, self.n_heads, self.head_dim]);
        let k = k.reshape([batch, seq_len, self.n_kv_heads, self.head_dim]);
        let v = v.reshape([batch, seq_len, self.n_kv_heads, self.head_dim]);

        // Apply RoPE to new Q, K (with correct positional offset)
        let (q, k) = rope.apply(q, k, offset);

        // Transpose to [batch, heads, seq, head_dim]
        let q = q.swap_dims(1, 2);
        let k = k.swap_dims(1, 2);
        let v = v.swap_dims(1, 2);

        // Update cache and get full K, V sequences
        let (k, v) = cache.update(k, v);

        // Total sequence length after cache update
        let total_seq_len = cache.seq_len();

        // Expand K, V for GQA if needed
        let (k, v) = self.expand_kv(k, v);

        // Compute attention scores: Q @ K^T * scale
        // Q: [batch, heads, seq_len, head_dim]
        // K: [batch, heads, total_seq_len, head_dim]
        // scores: [batch, heads, seq_len, total_seq_len]
        let k_t = k.swap_dims(2, 3);
        let scores = q.matmul(k_t) * self.scale;

        // Apply causal mask (accounts for different query/key lengths)
        let scores = if causal {
            self.apply_causal_mask_with_offset(scores, seq_len, total_seq_len, offset)
        } else {
            scores
        };

        // Apply sliding window mask if configured
        let scores = if let Some(window) = self.sliding_window {
            self.apply_sliding_window_mask_with_offset(
                scores,
                seq_len,
                total_seq_len,
                window,
                offset,
            )
        } else {
            scores
        };

        // Softmax
        let attn = softmax(scores, 3);

        // Apply attention: attn @ V
        let out = attn.matmul(v);

        // Transpose back and reshape
        let out = out.swap_dims(1, 2);
        let out = out.reshape([batch, seq_len, self.n_heads * self.head_dim]);

        // Output projection
        self.wo.forward(out)
    }

    /// Expand K, V heads for GQA (grouped-query attention).
    fn expand_kv(&self, k: Tensor<B, 4>, v: Tensor<B, 4>) -> (Tensor<B, 4>, Tensor<B, 4>) {
        if self.n_heads == self.n_kv_heads {
            return (k, v);
        }

        let repeat_factor = self.n_heads / self.n_kv_heads;
        let [batch, n_kv_heads, seq, head_dim] = k.dims();

        // Repeat: [batch, n_kv_heads, seq, head_dim] -> [batch, n_heads, seq, head_dim]
        let k = k
            .unsqueeze_dim::<5>(2) // [batch, n_kv_heads, 1, seq, head_dim]
            .repeat_dim(2, repeat_factor) // [batch, n_kv_heads, repeat, seq, head_dim]
            .reshape([batch, n_kv_heads * repeat_factor, seq, head_dim]);
        let v = v
            .unsqueeze_dim::<5>(2)
            .repeat_dim(2, repeat_factor)
            .reshape([batch, n_kv_heads * repeat_factor, seq, head_dim]);

        (k, v)
    }

    /// Apply causal mask to attention scores.
    fn apply_causal_mask(
        &self,
        scores: Tensor<B, 4>,
        seq_len: usize,
        _offset: usize,
    ) -> Tensor<B, 4> {
        super::masking::apply_causal_mask(scores, seq_len)
    }

    /// Apply sliding window mask to attention scores.
    fn apply_sliding_window_mask(
        &self,
        scores: Tensor<B, 4>,
        seq_len: usize,
        window: usize,
    ) -> Tensor<B, 4> {
        super::masking::apply_sliding_window_mask(scores, seq_len, window)
    }

    /// Apply causal mask with different query/key lengths (for KV cache).
    fn apply_causal_mask_with_offset(
        &self,
        scores: Tensor<B, 4>,
        q_len: usize,
        kv_len: usize,
        offset: usize,
    ) -> Tensor<B, 4> {
        super::masking::apply_causal_mask_with_offset(scores, q_len, kv_len, offset)
    }

    /// Apply sliding window mask with different query/key lengths (for KV cache).
    fn apply_sliding_window_mask_with_offset(
        &self,
        scores: Tensor<B, 4>,
        q_len: usize,
        kv_len: usize,
        window: usize,
        offset: usize,
    ) -> Tensor<B, 4> {
        super::masking::apply_sliding_window_mask_with_offset(scores, q_len, kv_len, window, offset)
    }
}

/// Create causal attention mask.
pub fn create_causal_mask<B: Backend>(seq_len: usize, device: &B::Device) -> Tensor<B, 4, Float> {
    let mut mask_data = vec![0.0f32; seq_len * seq_len];
    for i in 0..seq_len {
        for j in 0..seq_len {
            if j > i {
                mask_data[i * seq_len + j] = f32::NEG_INFINITY;
            }
        }
    }

    let mask: Tensor<B, 1> = Tensor::from_floats(mask_data.as_slice(), device);
    let mask: Tensor<B, 2> = mask.reshape([seq_len, seq_len]);
    mask.unsqueeze_dim::<3>(0).unsqueeze_dim(0)
}
