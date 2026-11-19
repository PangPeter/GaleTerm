//! Host key verification (known_hosts + TOFU).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};

use crate::error::{Error, Result};

/// Policy for unknown / changed host keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKeyPolicy {
    /// Prompt via [`HostKeyPrompt`] (default; Trust On First Use).
    #[default]
    Ask,
    /// Auto-learn unknown keys; still reject changed keys.
    AcceptNew,
    /// Reject unknown keys without prompting.
    Strict,
}

/// Decision returned by [`HostKeyPrompt`] for an unknown host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyDecision {
    /// Accept and append the key to known_hosts.
    AcceptAndLearn,
    /// Accept for this session only (do not write known_hosts).
    AcceptOnce,
    /// Refuse the connection.
    Reject,
}

/// Information shown to the user when a host key is unknown.
#[derive(Debug, Clone)]
pub struct HostKeyInfo {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    /// OpenSSH-style fingerprint, e.g. `SHA256:abcd…`.
    pub fingerprint: String,
}

impl HostKeyInfo {
    pub fn from_key(host: impl Into<String>, port: u16, key: &PublicKey) -> Self {
        Self {
            host: host.into(),
            port,
            algorithm: key.algorithm().to_string(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        }
    }

    pub fn display_host(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        }
    }
}

impl fmt::Display for HostKeyInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, {})",
            self.display_host(),
            self.algorithm,
            self.fingerprint
        )
    }
}

/// UI / CLI callback for first-time host key confirmation.
pub trait HostKeyPrompt: Send + Sync {
    fn confirm_unknown(&self, info: &HostKeyInfo) -> HostKeyDecision;
}

/// Always reject unknown hosts (used by [`HostKeyPolicy::Strict`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectUnknown;

impl HostKeyPrompt for RejectUnknown {
    fn confirm_unknown(&self, _info: &HostKeyInfo) -> HostKeyDecision {
        HostKeyDecision::Reject
    }
}

/// Always accept and learn (used by [`HostKeyPolicy::AcceptNew`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct AcceptAndLearnUnknown;

impl HostKeyPrompt for AcceptAndLearnUnknown {
    fn confirm_unknown(&self, _info: &HostKeyInfo) -> HostKeyDecision {
        HostKeyDecision::AcceptAndLearn
    }
}

/// Interactive stderr / stdin prompt for the CLI.
#[derive(Debug, Default, Clone, Copy)]
pub struct CliHostKeyPrompt;

impl HostKeyPrompt for CliHostKeyPrompt {
    fn confirm_unknown(&self, info: &HostKeyInfo) -> HostKeyDecision {
        eprintln!("The authenticity of host '{}' can't be established.", info.display_host());
        eprintln!("{} key fingerprint is {}.", info.algorithm, info.fingerprint);
        eprint!("Are you sure you want to continue connecting (yes/no/[fingerprint])? ");
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            return HostKeyDecision::Reject;
        }
        match input.trim() {
            "yes" | "y" => HostKeyDecision::AcceptAndLearn,
            "fingerprint" | "once" => HostKeyDecision::AcceptOnce,
            _ => HostKeyDecision::Reject,
        }
    }
}

/// Resolve the known_hosts file path (OpenSSH compatible).
pub fn default_known_hosts_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| Error::Ssh("cannot resolve home directory".into()))?;
    Ok(home.join(".ssh").join("known_hosts"))
}

/// Verify a server host key against known_hosts, applying [`HostKeyPolicy`].
pub fn verify_host_key(
    host: &str,
    port: u16,
    key_or_cert: &PublicKeyOrCertificate,
    known_hosts: &Path,
    policy: HostKeyPolicy,
    prompt: &dyn HostKeyPrompt,
) -> Result<bool> {
    let key = key_or_cert.public_key();
    let info = HostKeyInfo::from_key(host, port, &key);

    match check_known_hosts_path(host, port, &key, known_hosts) {
        Ok(true) => Ok(true),
        Ok(false) => {
            // No matching entry for this host — TOFU / policy.
            let decision = match policy {
                HostKeyPolicy::Ask => prompt.confirm_unknown(&info),
                HostKeyPolicy::AcceptNew => HostKeyDecision::AcceptAndLearn,
                HostKeyPolicy::Strict => HostKeyDecision::Reject,
            };
            match decision {
                HostKeyDecision::AcceptAndLearn => {
                    learn_known_hosts_path(host, port, &key, known_hosts)
                        .map_err(|e| Error::Ssh(format!("failed to write known_hosts: {e}")))?;
                    Ok(true)
                }
                HostKeyDecision::AcceptOnce => Ok(true),
                HostKeyDecision::Reject => Err(Error::HostKeyRejected {
                    host: info.display_host(),
                    fingerprint: info.fingerprint,
                }),
            }
        }
        Err(russh::keys::Error::KeyChanged { line }) => Err(Error::HostKeyChanged {
            host: info.display_host(),
            line,
            fingerprint: info.fingerprint,
            known_hosts: known_hosts.display().to_string(),
        }),
        Err(e) => Err(Error::Ssh(format!("known_hosts check failed: {e}"))),
    }
}

/// Helper used by the SSH handler: pick a prompt based on policy when none is supplied.
pub(crate) fn prompt_for_policy(
    policy: HostKeyPolicy,
    override_prompt: Option<Arc<dyn HostKeyPrompt>>,
) -> Arc<dyn HostKeyPrompt> {
    if let Some(p) = override_prompt {
        return p;
    }
    match policy {
        HostKeyPolicy::Ask => Arc::new(CliHostKeyPrompt),
        HostKeyPolicy::AcceptNew => Arc::new(AcceptAndLearnUnknown),
        HostKeyPolicy::Strict => Arc::new(RejectUnknown),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use russh::keys::{parse_public_key_base64, PublicKeyOrCertificate};
    use tempfile::tempdir;

    use super::*;

    const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn key(b64: &str) -> PublicKey {
        parse_public_key_base64(b64).unwrap()
    }

    struct RecordingPrompt {
        decision: HostKeyDecision,
        seen: Mutex<Option<HostKeyInfo>>,
    }

    impl HostKeyPrompt for RecordingPrompt {
        fn confirm_unknown(&self, info: &HostKeyInfo) -> HostKeyDecision {
            *self.seen.lock().unwrap() = Some(info.clone());
            self.decision
        }
    }

    #[test]
    fn learns_unknown_host_on_accept() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let pubkey = key(KEY_A);
        let prompt = RecordingPrompt {
            decision: HostKeyDecision::AcceptAndLearn,
            seen: Mutex::new(None),
        };

        let ok = verify_host_key(
            "example.test",
            22,
            &PublicKeyOrCertificate::from(pubkey.clone()),
            &path,
            HostKeyPolicy::Ask,
            &prompt,
        )
        .unwrap();
        assert!(ok);
        assert!(prompt.seen.lock().unwrap().is_some());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("example.test"));
        assert!(body.contains("ssh-ed25519"));

        // Second connect: known, no prompt.
        let prompt2 = RecordingPrompt {
            decision: HostKeyDecision::Reject,
            seen: Mutex::new(None),
        };
        let ok = verify_host_key(
            "example.test",
            22,
            &PublicKeyOrCertificate::from(pubkey),
            &path,
            HostKeyPolicy::Ask,
            &prompt2,
        )
        .unwrap();
        assert!(ok);
        assert!(prompt2.seen.lock().unwrap().is_none());
    }

    #[test]
    fn reject_changed_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");

        verify_host_key(
            "example.test",
            2222,
            &PublicKeyOrCertificate::from(key(KEY_A)),
            &path,
            HostKeyPolicy::AcceptNew,
            &AcceptAndLearnUnknown,
        )
        .unwrap();

        let err = verify_host_key(
            "example.test",
            2222,
            &PublicKeyOrCertificate::from(key(KEY_B)),
            &path,
            HostKeyPolicy::AcceptNew,
            &AcceptAndLearnUnknown,
        )
        .unwrap_err();

        match err {
            Error::HostKeyChanged { host, line, .. } => {
                assert_eq!(host, "[example.test]:2222");
                assert!(line >= 1);
            }
            other => panic!("expected HostKeyChanged, got {other}"),
        }
    }

    #[test]
    fn strict_rejects_unknown() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let err = verify_host_key(
            "strict.test",
            22,
            &PublicKeyOrCertificate::from(key(KEY_A)),
            &path,
            HostKeyPolicy::Strict,
            &RejectUnknown,
        )
        .unwrap_err();
        assert!(matches!(err, Error::HostKeyRejected { .. }));
        assert!(!path.exists() || fs::read_to_string(&path).unwrap().is_empty());
    }

    #[test]
    fn accept_once_does_not_write() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let prompt = RecordingPrompt {
            decision: HostKeyDecision::AcceptOnce,
            seen: Mutex::new(None),
        };
        assert!(verify_host_key(
            "once.test",
            22,
            &PublicKeyOrCertificate::from(key(KEY_A)),
            &path,
            HostKeyPolicy::Ask,
            &prompt,
        )
        .unwrap());
        assert!(!path.exists());
    }
}
