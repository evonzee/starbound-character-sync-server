pub mod api;
pub mod blobs;
pub mod db;
pub mod error;

use std::sync::Arc;

use sha2::{Digest, Sha256};

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Generate a fresh random bearer token (64 hex chars).
pub fn new_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode(b)
}

pub struct Config {
    /// Versions of each file to keep (history depth).
    pub history: i64,
    pub max_upload_bytes: usize,
    pub lease_ttl_secs: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self { history: 20, max_upload_bytes: 32 << 20, lease_ttl_secs: 600 }
    }
}

pub struct AppState {
    pub db: db::Db,
    pub blobs: blobs::BlobStore,
    pub cfg: Config,
    /// Serialises writes so blob GC can't race a concurrent upload of the same content.
    pub write_lock: tokio::sync::Mutex<()>,
}

pub type Shared = Arc<AppState>;
