//! Chunk embeddings from an OpenAI-compatible /v1/embeddings server, such as
//! llama.cpp's llama-server. A background loop embeds every chunk whose text
//! has no vector yet; search compares against an in-memory copy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use log::{info, warn};
use serde_json::{Value, json};

use crate::store::Store;

/// A slow server (an old APU does ~40 tokens/s) takes minutes per batch;
/// the request waits instead of timing out and resending the same work.
const BATCH_TIMEOUT: Duration = Duration::from_secs(600);
/// A search waits on the query vector; past this it goes full-text only.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Embedder {
    http: reqwest::Client,
    url: String,
    pub model: String,
    batch: usize,
}

pub struct Entry {
    pub chunk_id: i64,
    pub note_id: i64,
    pub obs_id: Option<i64>,
    pub vec: Vec<f32>,
}

/// The vectors search reads, rebuilt from the index when marked stale.
#[derive(Default)]
pub struct Vectors {
    entries: RwLock<Arc<Vec<Entry>>>,
    stale: AtomicBool,
}

impl Vectors {
    pub fn mark_stale(&self) {
        self.stale.store(true, Ordering::Release);
    }

    pub fn get(&self, store: &Store, model: &str) -> Result<Arc<Vec<Entry>>> {
        if self.stale.swap(false, Ordering::AcqRel) {
            let rows = store.vectors(model)?;
            let entries = rows.into_iter().map(|(chunk_id, note_id, obs_id, vec)| Entry { chunk_id, note_id, obs_id, vec }).collect();
            *self.entries.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(entries);
        }
        Ok(Arc::clone(&self.entries.read().unwrap_or_else(|e| e.into_inner())))
    }
}

pub fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

impl Embedder {
    pub fn new(url: String, model: String, batch: usize) -> Arc<Self> {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().expect("http client");
        Arc::new(Self { http, url: url.trim_end_matches('/').to_string(), model, batch: batch.max(1) })
    }

    async fn request(&self, texts: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>> {
        let resp: Value = self
            .http
            .post(format!("{}/v1/embeddings", self.url))
            .timeout(timeout)
            .json(&json!({ "model": self.model, "input": texts }))
            .send()
            .await
            .context("embedding server unreachable")?
            .error_for_status()?
            .json()
            .await?;
        let mut data: Vec<(u64, Vec<f32>)> = resp["data"]
            .as_array()
            .context("embedding response has no data")?
            .iter()
            .map(|d| {
                let v: Vec<f32> = d["embedding"].as_array().into_iter().flatten().filter_map(Value::as_f64).map(|x| x as f32).collect();
                (d["index"].as_u64().unwrap_or(0), normalize(v))
            })
            .collect();
        if data.len() != texts.len() || data.iter().any(|(_, v)| v.is_empty()) {
            bail!("embedding server returned {} vectors for {} texts", data.len(), texts.len());
        }
        data.sort_by_key(|(i, _)| *i);
        Ok(data.into_iter().map(|(_, v)| v).collect())
    }

    /// A few texts at once, with the query timeout: for work a client is waiting on.
    pub async fn embed_now(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.request(texts, QUERY_TIMEOUT).await
    }

    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.request(&[text.to_string()], QUERY_TIMEOUT).await?.remove(0))
    }

    /// Runs for the life of the process: embeds missing chunks, then polls.
    pub async fn backfill(self: Arc<Self>, store: Arc<Store>, vectors: Arc<Vectors>) {
        let mut done = 0usize;
        loop {
            let pending = match store.unembedded(&self.model, self.batch) {
                Ok(p) => p,
                Err(e) => {
                    warn!("embedding backfill: {e:#}");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            };
            if pending.is_empty() {
                if done > 0 {
                    info!("embedding backfill: {done} chunks embedded, index complete");
                    done = 0;
                    if let Err(e) = store.prune_vectors() {
                        warn!("pruning vectors: {e:#}");
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
            let texts: Vec<String> = pending.iter().map(|(_, t)| t.clone()).collect();
            match self.request(&texts, BATCH_TIMEOUT).await {
                Ok(vecs) => {
                    let items: Vec<(String, Vec<f32>)> = pending.into_iter().map(|(h, _)| h).zip(vecs).collect();
                    if let Err(e) = store.put_vectors(&self.model, &items) {
                        warn!("storing vectors: {e:#}");
                    }
                    done += items.len();
                    vectors.mark_stale();
                    if done % 200 < items.len() {
                        let (total, have) = store.embedding_counts(&self.model).unwrap_or_default();
                        info!("embedding backfill: {have}/{total} chunks");
                    }
                }
                Err(e) => {
                    warn!("embedding backfill: {e:#}");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        }
    }
}
