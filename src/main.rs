//! Muninn: an MCP server over a folder of Markdown notes. The notes are the
//! source of truth; the SQLite index next to them is rebuilt from the folder
//! and kept in step by a periodic scan.

mod embed;
mod mcp;
mod note;
mod search;
mod store;
mod vault;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use log::{info, warn};
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};

fn env(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let rt = tokio::runtime::Runtime::new()?;
    match std::env::args().nth(1).as_deref() {
        Some("healthcheck") => rt.block_on(healthcheck()),
        Some("index") => rt.block_on(index_only()),
        Some("version") => {
            println!("muninn-mcp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None | Some("serve") => rt.block_on(serve()),
        Some(other) => anyhow::bail!("unknown command {other:?}; use serve, index, healthcheck or version"),
    }
}

fn open_store() -> Result<Arc<store::Store>> {
    let root = PathBuf::from(env("MUNINN_NOTES_DIR", "/notes"));
    anyhow::ensure!(root.is_dir(), "MUNINN_NOTES_DIR {} is not a folder", root.display());
    let data = PathBuf::from(env("MUNINN_DATA_DIR", "/data"));
    std::fs::create_dir_all(&data).with_context(|| format!("creating {}", data.display()))?;
    Ok(Arc::new(store::Store::open(&data.join("muninn.db"), root, env("MUNINN_PROJECT", "main"))?))
}

fn embedder() -> Option<Arc<embed::Embedder>> {
    let url = env("EMBED_URL", "");
    (!url.is_empty()).then(|| embed::Embedder::new(url, env("EMBED_MODEL", "bge-m3"), env("EMBED_BATCH", "8").parse().unwrap_or(8)))
}

/// Indexes the folder and embeds every chunk, then exits: for building the
/// index on a faster machine and copying it over.
async fn index_only() -> Result<()> {
    let store = open_store()?;
    let changed = store.sync()?;
    info!("indexed: {changed} notes changed, {} notes in total", store.all_notes()?.len());
    let Some(e) = embedder() else { return Ok(()) };
    let vectors = Arc::new(embed::Vectors::default());
    let worker = tokio::spawn(Arc::clone(&e).backfill(Arc::clone(&store), vectors));
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let (total, done) = store.embedding_counts(&e.model)?;
        if done >= total {
            worker.abort();
            info!("embedded {done}/{total} chunks");
            return Ok(());
        }
    }
}

async fn serve() -> Result<()> {
    let store = open_store()?;
    let changed = store.sync()?;
    info!("{} notes in {} ({changed} re-indexed)", store.all_notes()?.len(), store.root.display());
    let vectors = Arc::new(embed::Vectors::default());
    vectors.mark_stale();
    let embedder = embedder();
    if let Some(e) = &embedder {
        tokio::spawn(Arc::clone(e).backfill(Arc::clone(&store), Arc::clone(&vectors)));
    }
    // Edits made outside the server (an editor, git, another tool) arrive by this scan.
    let scan_every = Duration::from_secs(env("MUNINN_SCAN_SECONDS", "15").parse().unwrap_or(15));
    {
        let store = Arc::clone(&store);
        let vectors = Arc::clone(&vectors);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(scan_every).await;
                let s = Arc::clone(&store);
                match tokio::task::spawn_blocking(move || s.sync()).await {
                    Ok(Ok(0)) => {}
                    Ok(Ok(n)) => {
                        info!("scan: {n} notes changed on disk");
                        vectors.mark_stale();
                    }
                    Ok(Err(e)) => warn!("scan: {e:#}"),
                    Err(e) => warn!("scan task: {e}"),
                }
            }
        });
    }
    let read_only = matches!(env("MUNINN_READ_ONLY", "").to_lowercase().as_str(), "1" | "true" | "yes");
    let project_id = std::env::var("MUNINN_PROJECT_ID").ok().filter(|v| !v.is_empty());
    let demoted = env("MUNINN_DEMOTE_CATEGORIES", "").split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
    let ctx = Arc::new(mcp::Ctx { store, embedder, vectors, read_only, project_id, demoted });
    let service = StreamableHttpService::new(
        move || Ok(mcp::Server::new(Arc::clone(&ctx))),
        LocalSessionManager::default().into(),
        // Clients may reach this behind a proxy under any host name, not only localhost.
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    );
    let addr: SocketAddr = format!("{}:{}", env("MCP_HOST", "0.0.0.0"), env("MCP_PORT", "8000")).parse()?;
    let app = Router::new().nest_service("/mcp", service);
    info!("MCP on http://{addr}/mcp{}", if read_only { " (read-only)" } else { "" });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tokio::select! {
        r = axum::serve(listener, app) => r.context("MCP server stopped"),
        _ = stop_signal() => {
            info!("shutting down");
            Ok(())
        }
    }
}

async fn stop_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// For the container HEALTHCHECK: the port accepts a connection.
async fn healthcheck() -> Result<()> {
    let addr = format!("127.0.0.1:{}", env("MCP_PORT", "8000"));
    tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(&addr))
        .await
        .context("timed out")?
        .with_context(|| format!("connecting to {addr}"))?;
    Ok(())
}
