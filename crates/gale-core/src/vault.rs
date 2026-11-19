//! Credential vault: secrets live in the OS store (macOS Keychain, Windows
//! Credential Manager, Secret Service on Linux), never in GaleTerm's files.
//!
//! The OS stores can't be enumerated portably, so a small index of *which*
//! credentials were saved (no secrets) sits next to `sessions.json` for the
//! Keychain page.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Service name the OS store files GaleTerm's entries under.
pub const SERVICE: &str = "GaleTerm";

/// What a secret unlocks; also its account name in the OS store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialKey(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Password,
    Passphrase,
}

impl CredentialKey {
    /// Login password, e.g. `ssh://root@example.com:22`.
    pub fn password(scheme: &str, user: &str, host: &str, port: u16) -> Self {
        Self(format!("{scheme}://{user}@{host}:{port}"))
    }

    /// Passphrase of a private key file.
    pub fn passphrase(key_path: &Path) -> Self {
        Self(format!("key:{}", key_path.display()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> CredentialKind {
        if self.0.starts_with("key:") {
            CredentialKind::Passphrase
        } else {
            CredentialKind::Password
        }
    }

    /// Human-readable target: `root@example.com:22` or the key path.
    pub fn target(&self) -> &str {
        match self.0.split_once("://") {
            Some((_, rest)) => rest,
            None => self.0.strip_prefix("key:").unwrap_or(&self.0),
        }
    }

    /// `ssh`, `ftp`, … for passwords; `key` for passphrases.
    pub fn scheme(&self) -> &str {
        match self.0.split_once("://") {
            Some((scheme, _)) => scheme,
            None => "key",
        }
    }
}

/// Backend holding the actual secrets.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    /// Removing a missing entry is not an error.
    fn delete(&self, account: &str) -> Result<()>;
}

/// The platform's credential store.
pub struct OsStore;

impl OsStore {
    fn entry(account: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, account).map_err(keychain_error)
    }
}

impl SecretStore for OsStore {
    fn get(&self, account: &str) -> Result<Option<String>> {
        match Self::entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keychain_error(e)),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        Self::entry(account)?
            .set_password(secret)
            .map_err(keychain_error)
    }

    fn delete(&self, account: &str) -> Result<()> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keychain_error(e)),
        }
    }
}

fn keychain_error(e: keyring::Error) -> Error {
    Error::Other(format!("system keychain: {e}"))
}

/// In-process store for tests and for platforms without a credential store.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>);

impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> Result<Option<String>> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).get(account).cloned())
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.into(), secret.into());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(account);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCredential {
    pub key: CredentialKey,
    pub saved_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Vault {
    store: Arc<dyn SecretStore>,
    index_path: PathBuf,
    /// Serializes index read-modify-write between threads.
    index_lock: Arc<Mutex<()>>,
}

impl Vault {
    pub fn default_index_path() -> Result<PathBuf> {
        let base = dirs::config_dir()
            .ok_or_else(|| Error::Other("cannot resolve user config directory".into()))?;
        Ok(base.join("gale-term").join("vault_index.json"))
    }

    /// The OS credential store with the default index file.
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(Arc::new(OsStore), Self::default_index_path()?))
    }

    pub fn new(store: Arc<dyn SecretStore>, index_path: impl Into<PathBuf>) -> Self {
        Self {
            store,
            index_path: index_path.into(),
            index_lock: Arc::new(Mutex::new(())),
        }
    }

    /// May block while the OS asks the user to allow access.
    pub fn get(&self, key: &CredentialKey) -> Result<Option<String>> {
        self.store.get(key.as_str())
    }

    pub fn set(&self, key: &CredentialKey, secret: &str) -> Result<()> {
        self.store.set(key.as_str(), secret)?;
        self.update_index(|list| {
            list.retain(|c| &c.key != key);
            list.push(SavedCredential {
                key: key.clone(),
                saved_at: Utc::now(),
            });
        })
    }

    pub fn delete(&self, key: &CredentialKey) -> Result<()> {
        self.store.delete(key.as_str())?;
        self.update_index(|list| list.retain(|c| &c.key != key))
    }

    /// Saved credentials, newest first.
    pub fn list(&self) -> Vec<SavedCredential> {
        let _guard = self.index_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut list = self.read_index();
        list.sort_by_key(|c| std::cmp::Reverse(c.saved_at));
        list
    }

    fn read_index(&self) -> Vec<SavedCredential> {
        std::fs::read(&self.index_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn update_index(&self, f: impl FnOnce(&mut Vec<SavedCredential>)) -> Result<()> {
        let _guard = self.index_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut list = self.read_index();
        f(&mut list);
        if let Some(parent) = self.index_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.index_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&list)?)?;
        std::fs::rename(&tmp, &self.index_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(dir: &Path) -> Vault {
        Vault::new(Arc::new(MemoryStore::default()), dir.join("vault_index.json"))
    }

    #[test]
    fn keys_describe_their_target() {
        let key = CredentialKey::password("ssh", "root", "example.com", 22);
        assert_eq!(key.as_str(), "ssh://root@example.com:22");
        assert_eq!(key.kind(), CredentialKind::Password);
        assert_eq!(key.scheme(), "ssh");
        assert_eq!(key.target(), "root@example.com:22");

        let key = CredentialKey::passphrase(Path::new("/home/me/.ssh/id_ed25519"));
        assert_eq!(key.kind(), CredentialKind::Passphrase);
        assert_eq!(key.scheme(), "key");
        assert_eq!(key.target(), "/home/me/.ssh/id_ed25519");
    }

    #[test]
    fn set_get_delete_keeps_index_without_secrets() {
        let dir = std::env::temp_dir().join(format!("gale-vault-{}", uuid::Uuid::new_v4()));
        let vault = vault(&dir);
        let a = CredentialKey::password("ssh", "u", "a", 22);
        let b = CredentialKey::password("ftp", "u", "b", 21);

        vault.set(&a, "pw-a").unwrap();
        vault.set(&b, "pw-b").unwrap();
        vault.set(&a, "pw-a2").unwrap();
        assert_eq!(vault.get(&a).unwrap().as_deref(), Some("pw-a2"));
        assert_eq!(vault.list().len(), 2);
        assert_eq!(vault.list()[0].key, a);

        let index = std::fs::read_to_string(dir.join("vault_index.json")).unwrap();
        assert!(!index.contains("pw-"));

        vault.delete(&a).unwrap();
        vault.delete(&a).unwrap();
        assert_eq!(vault.get(&a).unwrap(), None);
        assert_eq!(vault.list().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
