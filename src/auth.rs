use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use governor::{clock::DefaultClock, DefaultDirectRateLimiter, Quota, RateLimiter};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    num::NonZeroU32,
    sync::Mutex,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

pub struct AuthManager {
    username: String,
    password_hash: [u8; 32],
    sessions: Mutex<HashMap<[u8; 32], Instant>>,
    login_limiter: DefaultDirectRateLimiter,
}

impl AuthManager {
    pub fn from_env() -> Result<Self> {
        let username = std::env::var("GOS_USERNAME").map_err(|_| {
            anyhow::anyhow!("GOS_USERNAME must be configured before starting the web server")
        })?;
        let password = std::env::var("GOS_PASSWORD").map_err(|_| {
            anyhow::anyhow!("GOS_PASSWORD must be configured before starting the web server")
        })?;
        Self::new(username, &password)
    }

    pub fn new(username: String, password: &str) -> Result<Self> {
        if username.trim().is_empty() || username.len() > 128 {
            anyhow::bail!("GOS_USERNAME must contain between 1 and 128 characters");
        }
        validate_password(password)?;
        let digest = Sha256::digest(password.as_bytes());
        let mut password_hash = [0_u8; 32];
        password_hash.copy_from_slice(&digest);
        let quota = Quota::per_minute(NonZeroU32::new(5).expect("five is non-zero"));
        let login_limiter: RateLimiter<_, governor::state::InMemoryState, DefaultClock> =
            RateLimiter::direct(quota);
        Ok(Self {
            username,
            password_hash,
            sessions: Mutex::new(HashMap::new()),
            login_limiter,
        })
    }

    pub async fn authenticate(&self, username: &str, password: &str) -> Result<Option<String>> {
        self.login_limiter.until_ready().await;
        if username.len() > 128 || password.len() > 4096 {
            return Ok(None);
        }
        let username_matches = self
            .username
            .as_bytes()
            .ct_eq(username.as_bytes())
            .unwrap_u8()
            == 1;
        let digest = Sha256::digest(password.as_bytes());
        let mut password_hash = [0_u8; 32];
        password_hash.copy_from_slice(&digest);
        let password_matches = self.password_hash.ct_eq(&password_hash).unwrap_u8() == 1;
        if !username_matches || !password_matches {
            return Ok(None);
        }

        let mut token = [0_u8; 32];
        OsRng.fill_bytes(&mut token);
        let encoded = URL_SAFE_NO_PAD.encode(token);
        let fingerprint = fingerprint(&token);
        let expires_at = Instant::now() + SESSION_LIFETIME;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("authentication session lock poisoned"))?;
        sessions.retain(|_, expiration| *expiration > Instant::now());
        sessions.insert(fingerprint, expires_at);
        Ok(Some(encoded))
    }

    pub fn valid_session(&self, token: &str) -> Result<bool> {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(token) else {
            return Ok(false);
        };
        if decoded.len() != 32 {
            return Ok(false);
        }
        let fingerprint = fingerprint(&decoded);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("authentication session lock poisoned"))?;
        sessions.retain(|_, expiration| *expiration > Instant::now());
        Ok(sessions.contains_key(&fingerprint))
    }

    pub fn logout(&self, token: &str) -> Result<()> {
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(token) else {
            return Ok(());
        };
        if decoded.len() != 32 {
            return Ok(());
        }
        self.sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("authentication session lock poisoned"))?
            .remove(&fingerprint(&decoded));
        Ok(())
    }
}

fn validate_password(password: &str) -> Result<()> {
    if password.len() > 4096 {
        anyhow::bail!("GOS_PASSWORD must not exceed 4096 bytes");
    }
    let characters = password.chars().collect::<Vec<_>>();
    if characters.len() < 10
        || !characters.iter().any(|character| character.is_uppercase())
        || !characters.iter().any(|character| character.is_lowercase())
        || !characters
            .iter()
            .any(|character| character.is_ascii_digit())
        || !characters
            .iter()
            .any(|character| !character.is_alphanumeric() && !character.is_whitespace())
    {
        anyhow::bail!(
            "GOS_PASSWORD must be at least 10 characters and include an uppercase letter, a lowercase letter, a number, and a symbol"
        );
    }
    Ok(())
}

fn fingerprint(token: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(token);
    let mut result = [0_u8; 32];
    result.copy_from_slice(&digest);
    result
}

#[cfg(test)]
mod tests {
    use super::{validate_password, AuthManager};

    #[test]
    fn password_requires_length_and_each_character_class() {
        assert!(validate_password("GoodPassword1!").is_ok());
        assert!(validate_password("aA1!short").is_err());
        assert!(validate_password("lowercase123!").is_err());
        assert!(validate_password("UPPERCASE123!").is_err());
        assert!(validate_password("GoodPassword!!").is_err());
        assert!(validate_password("GoodPassword1").is_err());
    }

    #[tokio::test]
    async fn session_requires_credentials_and_expires_on_logout() {
        let auth = AuthManager::new("researcher".to_owned(), "A-strong-password-123").unwrap();
        assert!(auth
            .authenticate("researcher", "Wrong-password-1234")
            .await
            .unwrap()
            .is_none());
        let session = auth
            .authenticate("researcher", "A-strong-password-123")
            .await
            .unwrap()
            .expect("valid credentials create a session");
        assert!(auth.valid_session(&session).unwrap());
        auth.logout(&session).unwrap();
        assert!(!auth.valid_session(&session).unwrap());
    }
}
