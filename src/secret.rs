use anyhow::{Context, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::{rngs::OsRng, RngCore};
use std::path::{Path, PathBuf};

pub struct SecretCipher {
    cipher: ChaCha20Poly1305,
}

impl SecretCipher {
    pub fn load(database_path: &Path) -> Result<Self> {
        let key = if let Ok(master_key) = std::env::var("GOS_MASTER_KEY") {
            let decoded = hex::decode(master_key.trim())
                .context("GOS_MASTER_KEY must be a 64-character hexadecimal key")?;
            decoded
                .try_into()
                .map_err(|_| anyhow::anyhow!("GOS_MASTER_KEY must decode to exactly 32 bytes"))?
        } else {
            let parent = database_path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent)?;
            let key_path = parent.join(".encryption-key");
            load_or_create_key(&key_path)?
        };
        Ok(Self {
            cipher: ChaCha20Poly1305::new_from_slice(&key)
                .map_err(|_| anyhow::anyhow!("invalid API key encryption key"))?,
        })
    }

    pub fn encrypt(&self, value: &str) -> Result<String> {
        let mut nonce = [0_u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let encrypted = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), value.as_bytes())
            .map_err(|_| anyhow::anyhow!("encrypting API key"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend(encrypted);
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            bytes,
        ))
    }

    pub fn decrypt(&self, value: &str) -> Result<String> {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, value)
            .context("decoding encrypted API key")?;
        if bytes.len() < 13 {
            anyhow::bail!("encrypted API key value is malformed");
        }
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
            .map_err(|_| anyhow::anyhow!("decrypting API key; encryption key may have changed"))?;
        String::from_utf8(plaintext).context("decrypted API key is not valid UTF-8")
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32]> {
    if path.exists() {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        return bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("encryption key file must contain exactly 32 bytes"));
    }
    let mut key = [0_u8; 32];
    OsRng.fill_bytes(&mut key);
    create_key_file(path, &key)?;
    Ok(key)
}

fn create_key_file(path: &PathBuf, key: &[u8; 32]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(key)?;
    file.sync_all()?;
    Ok(())
}
