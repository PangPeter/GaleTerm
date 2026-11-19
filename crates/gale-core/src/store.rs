use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::session::{Protocol, SessionId, SessionProfile};

/// Maximum ProxyJump hops (excluding the final target).
pub const MAX_JUMP_DEPTH: usize = 5;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    sessions: Vec<SessionProfile>,
}

/// JSON-backed session store under the user config directory.
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
    data: StoreFile,
}

impl SessionStore {
    pub fn default_path() -> Result<PathBuf> {
        let base = dirs::config_dir().ok_or_else(|| {
            Error::Other("cannot resolve user config directory".into())
        })?;
        Ok(base.join("gale-term").join("sessions.json"))
    }

    pub fn open_default() -> Result<Self> {
        Self::open(Self::default_path()?)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let data = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            if raw.trim().is_empty() {
                StoreFile {
                    version: 1,
                    sessions: Vec::new(),
                }
            } else {
                serde_json::from_str(&raw)?
            }
        } else {
            StoreFile {
                version: 1,
                sessions: Vec::new(),
            }
        };

        Ok(Self { path, data })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> &[SessionProfile] {
        &self.data.sessions
    }

    pub fn get(&self, id: &str) -> Option<&SessionProfile> {
        self.data
            .sessions
            .iter()
            .find(|s| s.id.as_str() == id || s.name == id)
    }

    pub fn upsert(&mut self, profile: SessionProfile) -> Result<()> {
        if let Some(existing) = self
            .data
            .sessions
            .iter_mut()
            .find(|s| s.id.as_str() == profile.id.as_str())
        {
            *existing = profile;
        } else {
            self.data.sessions.push(profile);
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let before = self.data.sessions.len();
        self.data
            .sessions
            .retain(|s| s.id.as_str() != id && s.name != id);
        let removed = self.data.sessions.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.data)?;
        fs::write(&self.path, raw)?;
        Ok(())
    }

    pub fn next_id() -> SessionId {
        SessionId::new()
    }

    /// Resolve a ProxyJump chain ending at `id_or_name`.
    ///
    /// Returns `[jump1, jump2, …, target]` (at least one profile). Jump order
    /// is outermost first. Detects cycles and enforces [`MAX_JUMP_DEPTH`].
    pub fn resolve_jump_chain(&self, id_or_name: &str) -> Result<Vec<SessionProfile>> {
        let target = self
            .get(id_or_name)
            .cloned()
            .ok_or_else(|| Error::SessionNotFound(id_or_name.into()))?;

        if target.protocol != Protocol::Ssh {
            return Err(Error::InvalidSession(format!(
                "session `{}` is not ssh (got {})",
                id_or_name,
                target.protocol.as_str()
            )));
        }

        let mut chain_rev = vec![target.clone()];
        let mut seen = vec![target.id.as_str().to_string()];
        let mut current = target;

        while let Some(jump_id) = current
            .ssh
            .as_ref()
            .and_then(|s| s.jump_session_id.as_ref())
        {
            if chain_rev.len() - 1 >= MAX_JUMP_DEPTH {
                return Err(Error::JumpTooDeep(MAX_JUMP_DEPTH));
            }
            if seen.iter().any(|s| s == jump_id.as_str()) {
                return Err(Error::JumpCycle(jump_id.as_str().into()));
            }

            let jump = self.get(jump_id.as_str()).cloned().ok_or_else(|| {
                Error::SessionNotFound(format!(
                    "jump session `{}` (referenced by `{}`)",
                    jump_id,
                    current.name
                ))
            })?;

            if jump.protocol != Protocol::Ssh {
                return Err(Error::InvalidSession(format!(
                    "jump session `{}` is not ssh",
                    jump.name
                )));
            }
            if jump.ssh.is_none() {
                return Err(Error::InvalidSession(format!(
                    "jump session `{}` is missing ssh config",
                    jump.name
                )));
            }

            seen.push(jump.id.as_str().to_string());
            chain_rev.push(jump.clone());
            current = jump;
        }

        chain_rev.reverse();
        Ok(chain_rev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AuthMethod, SshAuth, SessionProfile};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store() -> SessionStore {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gale-term-test-{nanos}.json"));
        SessionStore::open(path).unwrap()
    }

    fn ssh_profile(name: &str, host: &str) -> SessionProfile {
        SessionProfile::new_ssh(
            name,
            host,
            "user",
            22,
            SshAuth {
                method: AuthMethod::PublicKey,
                password: None,
                private_key_path: None,
                passphrase: None,
            },
        )
    }

    #[test]
    fn upsert_list_remove() {
        let mut store = temp_store();
        let profile = ssh_profile("demo", "127.0.0.1");
        let id = profile.id.as_str().to_string();
        store.upsert(profile).unwrap();
        assert_eq!(store.list().len(), 1);
        assert!(store.get("demo").is_some());
        assert!(store.remove(&id).unwrap());
        assert!(store.list().is_empty());
        let _ = fs::remove_file(store.path());
    }

    #[test]
    fn resolve_jump_chain_order() {
        let mut store = temp_store();
        let bastion = ssh_profile("bastion", "1.1.1.1");
        let bastion_id = bastion.id.clone();
        store.upsert(bastion).unwrap();

        let mut prod = ssh_profile("prod", "10.0.0.5");
        prod.ssh.as_mut().unwrap().jump_session_id = Some(bastion_id);
        store.upsert(prod).unwrap();

        let chain = store.resolve_jump_chain("prod").unwrap();
        assert_eq!(
            chain
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bastion", "prod"]
        );
        let _ = fs::remove_file(store.path());
    }

    #[test]
    fn resolve_jump_cycle() {
        let mut store = temp_store();
        let mut a = ssh_profile("a", "1.1.1.1");
        let mut b = ssh_profile("b", "2.2.2.2");
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        a.ssh.as_mut().unwrap().jump_session_id = Some(b_id);
        b.ssh.as_mut().unwrap().jump_session_id = Some(a_id);
        store.upsert(a).unwrap();
        store.upsert(b).unwrap();

        let err = store.resolve_jump_chain("a").unwrap_err();
        assert!(matches!(err, Error::JumpCycle(_)));
        let _ = fs::remove_file(store.path());
    }

    #[test]
    fn resolve_missing_jump() {
        let mut store = temp_store();
        let mut prod = ssh_profile("prod", "10.0.0.5");
        prod.ssh.as_mut().unwrap().jump_session_id =
            Some(crate::session::SessionId("missing".into()));
        store.upsert(prod).unwrap();
        let err = store.resolve_jump_chain("prod").unwrap_err();
        assert!(matches!(err, Error::SessionNotFound(_)));
        let _ = fs::remove_file(store.path());
    }

    #[test]
    fn resolve_jump_too_deep() {
        let mut store = temp_store();
        let mut prev_id = None;
        for i in 0..=MAX_JUMP_DEPTH + 1 {
            let mut p = ssh_profile(&format!("h{i}"), &format!("10.0.0.{i}"));
            if let Some(id) = prev_id {
                p.ssh.as_mut().unwrap().jump_session_id = Some(id);
            }
            prev_id = Some(p.id.clone());
            let name = p.name.clone();
            store.upsert(p).unwrap();
            if i == MAX_JUMP_DEPTH + 1 {
                let err = store.resolve_jump_chain(&name).unwrap_err();
                assert!(matches!(err, Error::JumpTooDeep(_)));
            }
        }
        let _ = fs::remove_file(store.path());
    }
}
