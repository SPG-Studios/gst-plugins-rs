//! Language Model Decoder for Voxtral.
//!
//! Ministral-3B based decoder with GQA and sliding window attention.

use burn::config::Config;
use burn::module::{Module, Param, ParamId};
use burn::nn::{Embedding, EmbeddingConfig};
use burn::tensor::Tensor;
use burn::tensor::backend::Backend;

use super::layers::{
    DecoderLayer, DecoderLayerConfig, LayerCaches, RmsNorm, RmsNormConfig, RoPE, RoPEConfig,
};

/// Decomposed parts of a LanguageModel, for per-layer streaming serialization.
///
/// RoPE is excluded because it contains no learned parameters — it's
/// recomputed from config at init time.
pub struct DecoderParts<B: Backend> {
    pub tok_embeddings: Embedding<B>,
    pub layers: Vec<DecoderLayer<B>>,
    pub norm: RmsNorm<B>,
    pub d_model: usize,
}

/// Language model configuration.
#[derive(Config, Debug)]
pub struct LanguageModelConfig {
    /// Vocabulary size.
    pub vocab_size: usize,
    /// Model dimension.
    pub d_model: usize,
    /// Number of transformer layers.
    pub n_layers: usize,
    /// Number of query heads.
    pub n_heads: usize,
    /// Number of KV heads (for GQA).
    #[config(default = 8)]
    pub n_kv_heads: usize,
    /// Head dimension.
    #[config(default = 128)]
    pub head_dim: usize,
    /// MLP hidden dimension.
    #[config(default = 9216)]
    pub mlp_hidden_dim: usize,
    /// Temporal conditioning dimension for ADA RMSNorm.
    #[config(default = 32)]
    pub t_cond_dim: usize,
    /// Sliding window size for attention.
    pub sliding_window: Option<usize>,
    /// Maximum sequence length for RoPE.
    #[config(default = 16384)]
    pub max_seq_len: usize,
    /// RoPE theta.
    #[config(default = 1_000_000.0)]
    pub rope_theta: f64,
    /// RMSNorm epsilon.
    #[config(default = 1e-5)]
    pub norm_eps: f64,
}

impl LanguageModelConfig {
    /// Create a config from the Voxtral model defaults.
    pub fn voxtral() -> Self {
        Self::new(131072, 3072, 26, 32).with_sliding_window(Some(8192))
    }
}

/// Language model decoder module.
///
/// Architecture:
/// 1. Token embeddings (tied with LM head)
/// 2. 26 transformer layers with:
///    - ADA RMSNorm (t-conditioned)
///    - GQA attention (32Q/8KV) with sliding window (8192)
///    - Standard RMSNorm
///    - SwiGLU MLP (no biases)
/// 3. Final RMSNorm
/// 4. LM head (tied to embeddings)
///
/// Input: Token IDs [batch, seq]
/// Output: Logits [batch, seq, vocab_size]
#[derive(Module, Debug)]
pub struct LanguageModel<B: Backend> {
    /// Token embeddings (tied with LM head).
    tok_embeddings: Embedding<B>,
    /// Rotary position embeddings.
    rope: RoPE<B>,
    /// Transformer layers.
    layers: Vec<DecoderLayer<B>>,
    /// Final normalization.
    norm: RmsNorm<B>,
    /// Model dimension (for LM head).
    d_model: usize,
    /// Number of KV heads (for pre-allocated cache sizing).
    n_kv_heads: usize,
    /// Head dimension (for pre-allocated cache sizing).
    head_dim: usize,
}

impl LanguageModelConfig {
    /// Initialize just the token embedding layer.
    pub fn init_embeddings<B: Backend>(&self, device: &B::Device) -> Embedding<B> {
        EmbeddingConfig::new(self.vocab_size, self.d_model).init(device)
    }

    /// Initialize a single decoder transformer layer.
    pub fn init_single_layer<B: Backend>(&self, device: &B::Device) -> DecoderLayer<B> {
        DecoderLayerConfig::new(
            self.d_model,
            self.n_heads,
            self.n_kv_heads,
            self.head_dim,
            self.mlp_hidden_dim,
            self.t_cond_dim,
        )
        .with_sliding_window(self.sliding_window)
        .with_norm_eps(self.norm_eps)
        .init(device)
    }

    /// Initialize just the final RMS normalization layer.
    pub fn init_norm<B: Backend>(&self, device: &B::Device) -> RmsNorm<B> {
        RmsNormConfig::new(self.d_model)
            .with_eps(self.norm_eps)
            .init(device)
    }

    /// Initialize RoPE (rotary position embeddings).
    ///
    /// RoPE has no learned parameters — it's computed from config.
    pub fn init_rope<B: Backend>(&self, device: &B::Device) -> RoPE<B> {
        RoPEConfig::new(self.head_dim, self.max_seq_len)
            .with_theta(self.rope_theta)
            .init(device)
    }

    /// Initialize the language model.
    pub fn init<B: Backend>(&self, device: &B::Device) -> LanguageModel<B> {
        let tok_embeddings = EmbeddingConfig::new(self.vocab_size, self.d_model).init(device);

        let rope = RoPEConfig::new(self.head_dim, self.max_seq_len)
            .with_theta(self.rope_theta)
            .init(device);

        let layers = (0..self.n_layers)
            .map(|_| {
                DecoderLayerConfig::new(
                    self.d_model,
                    self.n_heads,
                    self.n_kv_heads,
                    self.head_dim,
                    self.mlp_hidden_dim,
                    self.t_cond_dim,
                )
                .with_sliding_window(self.sliding_window)
                .with_norm_eps(self.norm_eps)
                .init(device)
            })
            .collect();

        let norm = RmsNormConfig::new(self.d_model)
            .with_eps(self.norm_eps)
            .init(device);

        LanguageModel {
            tok_embeddings,
            rope,
            layers,
            norm,
            d_model: self.d_model,
            n_kv_heads: self.n_kv_heads,
            head_dim: self.head_dim,
        }
    }
}

impl<B: Backend> LanguageModel<B> {
    /// Create language model from components (for weight loading).
    pub fn new(
        tok_embeddings_weight: Tensor<B, 2>,
        rope: RoPE<B>,
        layers: Vec<DecoderLayer<B>>,
        final_norm_weight: Tensor<B, 1>,
        eps: f64,
    ) -> Self {
        let d_model = tok_embeddings_weight.dims()[1];

        let tok_embeddings = Embedding {
            weight: Param::initialized(ParamId::new(), tok_embeddings_weight),
        };

        let norm = RmsNorm {
            weight: burn::nn::RmsNorm {
                gamma: Param::initialized(ParamId::new(), final_norm_weight),
                epsilon: eps,
            },
        };

        // Voxtral decoder: 32 Q heads / 8 KV heads, head_dim = 128
        let n_kv_heads = 8;
        let head_dim = 128;

        Self {
            tok_embeddings,
            rope,
            layers,
            norm,
            d_model,
            n_kv_heads,
            head_dim,
        }
    }

    /// Forward pass returning hidden states (before LM head).
    ///
    /// # Arguments
    /// * `token_ids` - Token IDs [batch, seq]
    /// * `t_embed` - Temporal embedding [batch, 1, d_model]
    /// * `offset` - Position offset for KV cache
    ///
    /// # Returns
    /// Hidden states [batch, seq, d_model]
    pub fn forward(
        &self,
        token_ids: Tensor<B, 2, burn::tensor::Int>,
        t_embed: Tensor<B, 3>,
        offset: usize,
    ) -> Tensor<B, 3> {
        // Token embeddings
        let x = self.tok_embeddings.forward(token_ids);

        // Transformer layers
        let mut x = x;
        for layer in &self.layers {
            x = layer.forward(x, t_embed.clone(), &self.rope, offset);
        }

        // Final normalization
        self.norm.forward(x)
    }

    /// Get token embeddings (for adding to audio embeddings in multimodal mode).
    ///
    /// # Arguments
    /// * `token_ids` - Token IDs [batch, seq]
    ///
    /// # Returns
    /// Token embeddings [batch, seq, d_model]
    pub fn embed_tokens(&self, token_ids: Tensor<B, 2, burn::tensor::Int>) -> Tensor<B, 3> {
        self.tok_embeddings.forward(token_ids)
    }

    /// Forward pass with hidden states input (for multimodal).
    ///
    /// # Arguments
    /// * `hidden_states` - Hidden states [batch, seq, d_model]
    /// * `t_embed` - Temporal embedding [batch, 1, d_model]
    /// * `offset` - Position offset for KV cache
    ///
    /// # Returns
    /// Hidden states [batch, seq, d_model]
    pub fn forward_hidden(
        &self,
        hidden_states: Tensor<B, 3>,
        t_embed: Tensor<B, 3>,
        offset: usize,
    ) -> Tensor<B, 3> {
        let mut x = hidden_states;
        for layer in &self.layers {
            x = layer.forward(x, t_embed.clone(), &self.rope, offset);
        }
        self.norm.forward(x)
    }

    /// Compute logits from hidden states (LM head with tied embeddings).
    ///
    /// # Arguments
    /// * `hidden_states` - Hidden states [batch, seq, d_model]
    ///
    /// # Returns
    /// Logits [batch, seq, vocab_size]
    pub fn lm_head(&self, hidden_states: Tensor<B, 3>) -> Tensor<B, 3> {
        // Tied embeddings: logits = hidden @ embeddings.T
        let [batch, seq, _d_model] = hidden_states.dims();

        // Get embedding weights [vocab_size, d_model]
        let embed_weights = self.tok_embeddings.weight.val();
        let vocab_size = embed_weights.dims()[0];

        // Compute logits: [batch, seq, d_model] @ [d_model, vocab_size] -> [batch, seq, vocab_size]
        let embed_t = embed_weights.transpose().unsqueeze::<3>(); // [1, d_model, vocab_size]
        let logits = hidden_states.matmul(embed_t);

        // Result shape should be [batch, seq, vocab_size]
        logits.reshape([batch, seq, vocab_size])
    }

    /// Forward pass with KV cache (for autoregressive generation).
    ///
    /// # Arguments
    /// * `token_ids` - Token IDs [batch, seq]
    /// * `t_embed` - Temporal embedding [batch, 1, d_model]
    /// * `caches` - KV caches for all layers
    ///
    /// # Returns
    /// Hidden states [batch, seq, d_model]
    pub fn forward_with_cache(
        &self,
        token_ids: Tensor<B, 2, burn::tensor::Int>,
        t_embed: Tensor<B, 3>,
        caches: &mut LayerCaches<B>,
    ) -> Tensor<B, 3> {
        let x = self.tok_embeddings.forward(token_ids);

        let mut x = x;
        for (i, layer) in self.layers.iter().enumerate() {
            if let Some(cache) = caches.get_mut(i) {
                x = layer.forward_with_cache(x, t_embed.clone(), &self.rope, cache);
            }
        }

        self.norm.forward(x)
    }

    /// Forward pass with hidden states input and KV cache.
    ///
    /// # Arguments
    /// * `hidden_states` - Hidden states [batch, seq, d_model]
    /// * `t_embed` - Temporal embedding [batch, 1, d_model]
    /// * `caches` - KV caches for all layers
    ///
    /// # Returns
    /// Hidden states [batch, seq, d_model]
    pub fn forward_hidden_with_cache(
        &self,
        hidden_states: Tensor<B, 3>,
        t_embed: Tensor<B, 3>,
        caches: &mut LayerCaches<B>,
    ) -> Tensor<B, 3> {
        let mut x = hidden_states;
        for (i, layer) in self.layers.iter().enumerate() {
            if let Some(cache) = caches.get_mut(i) {
                x = layer.forward_with_cache(x, t_embed.clone(), &self.rope, cache);
            }
        }
        self.norm.forward(x)
    }

    /// Get the number of layers.
    pub fn n_layers(&self) -> usize {
        self.layers.len()
    }

    /// Get the model dimension.
    pub fn d_model(&self) -> usize {
        self.d_model
    }

    /// Create a new cache for this decoder.
    pub fn create_cache(&self) -> LayerCaches<B> {
        LayerCaches::new(self.layers.len())
    }

    /// Create a pre-allocated cache sized for the given max sequence length.
    pub fn create_cache_preallocated(&self, max_seq: usize, device: &B::Device) -> LayerCaches<B> {
        LayerCaches::new_preallocated(
            self.layers.len(),
            1, // batch = 1
            self.n_kv_heads,
            max_seq,
            self.head_dim,
            device,
        )
    }

    /// Decompose the decoder into its parts for per-layer serialization.
    ///
    /// RoPE is excluded (recomputed from config, no learned weights).
    pub fn into_parts(self) -> DecoderParts<B> {
        DecoderParts {
            tok_embeddings: self.tok_embeddings,
            layers: self.layers,
            norm: self.norm,
            d_model: self.d_model,
        }
    }

    /// Assemble a decoder from individually loaded parts.
    ///
    /// `rope` must be initialized separately from config since it has no
    /// learned parameters and is not serialized.
    pub fn from_parts(parts: DecoderParts<B>, rope: RoPE<B>) -> Self {
        let n_kv_heads = 8;
        let head_dim = 128;
        Self {
            tok_embeddings: parts.tok_embeddings,
            rope,
            layers: parts.layers,
            norm: parts.norm,
            d_model: parts.d_model,
            n_kv_heads,
            head_dim,
        }
    }
}
