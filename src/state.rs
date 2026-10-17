use crate::{config::Config, db::Database, gemini_client::GeminiClient, secret::SecretCipher};
use tokio::sync::{broadcast, mpsc};

pub struct AppState {
    pub config: Config,
    pub db: Database,
    pub cipher: SecretCipher,
    pub gemini: GeminiClient,
    pub queue_tx: mpsc::Sender<()>,
    pub progress_tx: broadcast::Sender<String>,
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
