use crate::{
    auth::AuthManager, config::Config, db::Database, gemini_client::GeminiClient,
    secret::SecretCipher,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

pub struct AppState {
    pub config: Config,
    pub auth: Option<Arc<AuthManager>>,
    pub db: Database,
    pub cipher: SecretCipher,
    pub gemini: GeminiClient,
    pub queue_tx: mpsc::Sender<()>,
    pub progress_tx: broadcast::Sender<String>,
    pub scan_cancellation: Mutex<CancellationToken>,
}

pub fn api_key(state: &AppState) -> anyhow::Result<String> {
    if let Some(encrypted) = state.db.setting("gemini_api_key")? {
        let key = state.cipher.decrypt(&encrypted)?;
        if !key.trim().is_empty() {
            return Ok(key);
        }
    }
    state
        .config
        .gemini_api_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Gemini API key is not configured"))
}
