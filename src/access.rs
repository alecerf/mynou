//! The API access token. Mynou creates it once, on the first visit of the web
//! interface or with `mynou token --reset`, shows it once, and keeps only its
//! SHA-256 digest, so the token can never be read back from storage.
use crate::{
    Result,
    crypto::{constant_time_eq, random_bytes, sha256},
};
use std::sync::{Arc, RwLock};

/// The digest the API checks bearer tokens against, shared by the startup and
/// full listeners. Absent until the first token is created.
pub struct Access(RwLock<Option<[u8; 32]>>);

impl Access {
    pub fn new(digest: Option<[u8; 32]>) -> Arc<Self> {
        Arc::new(Self(RwLock::new(digest)))
    }

    /// An access set to a token chosen by the caller.
    pub fn fixed(token: &str) -> Result<Arc<Self>> {
        if token.len() < 32 || token.len() > 4096 || token.chars().any(char::is_control) {
            return Err(
                "The API token must contain 32 to 4096 bytes and no control characters".into(),
            );
        }
        Ok(Self::new(Some(sha256(token.as_bytes()))))
    }

    pub fn allows(&self, offered: &[u8]) -> bool {
        match *self.0.read().unwrap_or_else(|e| e.into_inner()) {
            Some(digest) => constant_time_eq(&sha256(offered), &digest),
            None => false,
        }
    }

    pub fn is_set(&self) -> bool {
        self.0.read().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub(crate) fn replace(&self, digest: Option<[u8; 32]>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = digest;
    }

    /// Creates the first token: `save` persists its digest before the token is
    /// returned. None when a token already exists, so only the first claim wins.
    pub(crate) fn claim(
        &self,
        save: impl FnOnce(&[u8; 32]) -> Result<()>,
    ) -> Result<Option<String>> {
        let mut current = self.0.write().unwrap_or_else(|e| e.into_inner());
        if current.is_some() {
            return Ok(None);
        }
        let (token, digest) = generate()?;
        save(&digest)?;
        *current = Some(digest);
        Ok(Some(token))
    }
}

/// The token digest saved in a stopped installation's store, if any.
pub fn stored(store_dir: &std::path::Path) -> Result<Option<[u8; 32]>> {
    if !crate::store::Store::directory_exists(store_dir)? {
        return Ok(None);
    }
    crate::store::Store::open_read_only(store_dir)?.access()
}

/// A new random token and its digest.
pub fn generate() -> Result<(String, [u8; 32])> {
    let token = hex(&random_bytes::<32>()?);
    let digest = sha256(token.as_bytes());
    Ok((token, digest))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn digest_from_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (n, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * n..2 * n + 2], 16).ok()?;
    }
    Some(digest)
}

/// Whether a `Host` header names this machine literally. A setup request for
/// any other name may come from a page that rebound its domain to loopback.
pub(crate) fn local_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(name, "127.0.0.1" | "[::1]") || name.eq_ignore_ascii_case("localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_claim_creates_a_token_and_only_its_digest_is_kept() {
        let access = Access::new(None);
        assert!(!access.allows(b""));
        let mut saved = None;
        let token = access
            .claim(|digest| {
                saved = Some(*digest);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(saved, Some(sha256(token.as_bytes())));
        assert!(access.allows(token.as_bytes()));
        assert!(!access.allows(b"another token of sufficient length 0123"));
        assert!(access.claim(|_| Ok(())).unwrap().is_none());
        assert_eq!(digest_from_hex(&hex(&saved.unwrap())), saved);
        assert!(digest_from_hex("zz").is_none());
    }

    #[test]
    fn a_failed_save_leaves_the_setup_open() {
        let access = Access::new(None);
        assert!(access.claim(|_| Err("disk full".into())).is_err());
        assert!(!access.is_set());
    }

    #[test]
    fn only_literal_loopback_hosts_may_set_up() {
        for host in [
            "127.0.0.1:8787",
            "localhost:8787",
            "LOCALHOST",
            "[::1]:8787",
        ] {
            assert!(local_host(host), "{host}");
        }
        for host in [
            "",
            "evil.example:8787",
            "127.0.0.1.evil.example",
            "[::1]x",
            "192.168.1.2:8787",
        ] {
            assert!(!local_host(host), "{host}");
        }
    }
}
