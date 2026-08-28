//! Local embedding provider — pure-Rust BERT inference via candle.
//!
//! Replaces the previous fastembed (onnxruntime) implementation while
//! reproducing its exact pipeline, so vectors stay compatible with those
//! already stored:
//!
//! * tokenizer.json tokenization, truncation to 512 tokens, batch-longest
//!   padding (fastembed's `DEFAULT_MAX_LENGTH` + `PaddingStrategy::BatchLongest`)
//! * no query/passage prefixes — fastembed's `embed()` passes texts verbatim
//! * CLS-token pooling for BGE models, masked mean pooling for MiniLM
//!   (fastembed's `get_default_pooling_method`)
//! * L2 normalization with a 1e-12 epsilon (fastembed's `normalize`)

use anyhow::{Context, Result};
use async_trait::async_trait;
use candle::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::embedding::{Embedding, EmbeddingProvider};

/// Max tokens per input; matches fastembed's `DEFAULT_MAX_LENGTH` (the BERT
/// position-embedding limit for all supported models).
const MAX_LENGTH: usize = 512;

/// Inputs per forward pass. Padding is per-batch and masked out, so chunking
/// does not affect the resulting vectors.
const BATCH_SIZE: usize = 16;

#[derive(Clone, Copy)]
enum Pooling {
    /// First (CLS) token of the last hidden state — BGE models.
    Cls,
    /// Attention-mask-weighted mean over tokens — sentence-transformers MiniLM.
    Mean,
}

pub struct LocalEmbeddingProvider {
    model: BertModel,
    tokenizer: Tokenizer,
    pooling: Pooling,
    model_id: String,
    dimensions: usize,
    device: Device,
}

impl LocalEmbeddingProvider {
    /// Create a local embedding provider. Downloads the model from the
    /// Hugging Face hub on first use (cached under `~/.cache/huggingface`).
    pub fn new(model_name: &str) -> Result<Self> {
        let (repo, dimensions, pooling) = match model_name {
            "bge-small-en-v1.5" => ("BAAI/bge-small-en-v1.5", 384, Pooling::Cls),
            "bge-base-en-v1.5" => ("BAAI/bge-base-en-v1.5", 768, Pooling::Cls),
            "bge-large-en-v1.5" => ("BAAI/bge-large-en-v1.5", 1024, Pooling::Cls),
            "all-minilm-l6-v2" | "all-minilm" => {
                ("sentence-transformers/all-MiniLM-L6-v2", 384, Pooling::Mean)
            }
            _ => {
                tracing::warn!(model = model_name, "unknown local model, defaulting to bge-small-en-v1.5");
                ("BAAI/bge-small-en-v1.5", 384, Pooling::Cls)
            }
        };

        let api = hf_hub::api::sync::Api::new().context("hf-hub api init")?;
        let repo_api = api.model(repo.to_string());
        let config_path = repo_api.get("config.json").context("fetch config.json")?;
        let tokenizer_path = repo_api.get("tokenizer.json").context("fetch tokenizer.json")?;
        let weights_path = repo_api
            .get("model.safetensors")
            .context("fetch model.safetensors")?;

        let config: Config = serde_json::from_str(&std::fs::read_to_string(&config_path)?)
            .context("parse config.json")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer.json: {e}"))?;
        tokenizer
            .with_padding(Some(PaddingParams {
                strategy: PaddingStrategy::BatchLongest,
                ..Default::default()
            }))
            .with_truncation(Some(TruncationParams {
                max_length: MAX_LENGTH,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configure tokenizer: {e}"))?;

        // Embeddings run in the background; CPU keeps this dependency-free.
        let device = Device::Cpu;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)
                .context("load model.safetensors")?
        };
        let model = BertModel::load(vb, &config).context("build BERT model")?;

        Ok(Self {
            model,
            tokenizer,
            pooling,
            model_id: format!("local/{}", model_name),
            dimensions,
            device,
        })
    }

    /// Embed one already-tokenized batch and return unit-length vectors.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenize batch: {e}"))?;

        let batch = encodings.len();
        let seq_len = encodings[0].len();
        let mut ids = Vec::with_capacity(batch * seq_len);
        let mut type_ids = Vec::with_capacity(batch * seq_len);
        let mut mask = Vec::with_capacity(batch * seq_len);
        for encoding in &encodings {
            ids.extend_from_slice(encoding.get_ids());
            type_ids.extend_from_slice(encoding.get_type_ids());
            mask.extend_from_slice(encoding.get_attention_mask());
        }

        let ids = Tensor::from_vec(ids, (batch, seq_len), &self.device)?;
        let type_ids = Tensor::from_vec(type_ids, (batch, seq_len), &self.device)?;
        let mask = Tensor::from_vec(mask, (batch, seq_len), &self.device)?;

        // (batch, seq_len, hidden)
        let hidden = self.model.forward(&ids, &type_ids, Some(&mask))?;

        let pooled = match self.pooling {
            Pooling::Cls => hidden.narrow(1, 0, 1)?.squeeze(1)?,
            Pooling::Mean => {
                let mask = mask.to_dtype(DTYPE)?.unsqueeze(2)?; // (batch, seq_len, 1)
                let summed = hidden.broadcast_mul(&mask)?.sum(1)?; // (batch, hidden)
                let counts = mask.sum(1)?.clamp(1e-9, f64::INFINITY)?; // (batch, 1)
                summed.broadcast_div(&counts)?
            }
        };

        let vectors: Vec<Vec<f32>> = pooled.to_vec2()?;
        Ok(vectors.into_iter().map(|v| l2_normalize(&v)).collect())
    }
}

/// L2 normalization, identical to fastembed's `normalize` (epsilon included).
fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let epsilon = 1e-12;
    v.iter().map(|&x| x / (norm + epsilon)).collect()
}

#[async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Embedding>> {
        // CPU inference is fast for these small models (~tens of ms per
        // chunk); run synchronously like the previous implementation did.
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(BATCH_SIZE) {
            for vector in self.embed_batch(chunk)? {
                out.push(Embedding { vector });
            }
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}
