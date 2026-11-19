//! SSH agent authentication (`SSH_AUTH_SOCK` / Pageant).

use russh::client::Handle;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{Algorithm, HashAlg};

use crate::client::Handler;
use crate::error::{Error, Result};

type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// Connect to the platform SSH agent.
///
/// - Unix: `SSH_AUTH_SOCK` Unix domain socket
/// - Windows: `SSH_AUTH_SOCK` named pipe if set, otherwise Pageant
pub async fn connect_agent() -> Result<DynAgent> {
    #[cfg(unix)]
    {
        AgentClient::connect_env()
            .await
            .map(|c| c.dynamic())
            .map_err(map_agent_connect_error)
    }

    #[cfg(windows)]
    {
        if let Ok(sock) = std::env::var("SSH_AUTH_SOCK") {
            if !sock.is_empty() {
                return AgentClient::connect_named_pipe(&sock)
                    .await
                    .map(|c| c.dynamic())
                    .map_err(map_agent_connect_error);
            }
        }
        AgentClient::connect_pageant()
            .await
            .map(|c| c.dynamic())
            .map_err(map_agent_connect_error)
    }

    #[cfg(not(any(unix, windows)))]
    {
        Err(Error::Agent(
            "SSH agent is not supported on this platform".into(),
        ))
    }
}

fn map_agent_connect_error(e: russh::keys::Error) -> Error {
    match e {
        russh::keys::Error::EnvVar("SSH_AUTH_SOCK") => Error::Agent(
            "SSH_AUTH_SOCK is not set; start ssh-agent and add keys (ssh-add)".into(),
        ),
        russh::keys::Error::BadAuthSock => Error::Agent(
            "SSH_AUTH_SOCK points to a missing socket; is ssh-agent running?".into(),
        ),
        other => Error::Agent(format!("failed to connect to SSH agent: {other}")),
    }
}

fn rsa_hash_alg(key_alg: &Algorithm, preferred: Option<HashAlg>) -> Option<HashAlg> {
    match key_alg {
        Algorithm::Rsa { .. } => preferred,
        _ => None,
    }
}

/// Try each identity from the agent until one authenticates successfully.
pub async fn authenticate_with_agent(
    handle: &mut Handle<Handler>,
    username: &str,
) -> Result<bool> {
    let mut agent = connect_agent().await?;

    // Destination-constrained keys require session-bind; ignore if agent
    // does not support the extension (common with older agents).
    if let Some(binding) = handle.agent_session_binding() {
        if let Err(e) = agent.bind_session(binding, false).await {
            tracing::debug!("agent session-bind skipped: {e}");
        }
    }

    let identities = agent
        .request_identities()
        .await
        .map_err(|e| Error::Agent(format!("failed to list agent identities: {e}")))?;

    if identities.is_empty() {
        return Err(Error::Agent(
            "SSH agent has no identities; run ssh-add".into(),
        ));
    }

    let preferred_rsa = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| Error::Ssh(e.to_string()))?
        .flatten();

    let mut last_signer_err: Option<String> = None;

    for identity in identities {
        let result = match identity {
            AgentIdentity::PublicKey { key, comment } => {
                let key_alg = key.algorithm();
                let hash_alg = rsa_hash_alg(&key_alg, preferred_rsa);
                tracing::debug!(
                    comment = %comment,
                    alg = %key_alg,
                    "trying agent public key"
                );
                handle
                    .authenticate_publickey_with(username, key, hash_alg, &mut agent)
                    .await
            }
            AgentIdentity::Certificate {
                certificate,
                comment,
            } => {
                let cert_alg = certificate.algorithm();
                let hash_alg = rsa_hash_alg(&cert_alg, preferred_rsa);
                tracing::debug!(
                    comment = %comment,
                    alg = %cert_alg,
                    "trying agent certificate"
                );
                handle
                    .authenticate_certificate_with(username, certificate, hash_alg, &mut agent)
                    .await
            }
        };

        match result {
            Ok(auth) if auth.success() => return Ok(true),
            Ok(_) => continue,
            Err(e) => {
                // Signer declined / key missing in agent for this attempt —
                // keep going with remaining identities.
                tracing::debug!("agent identity rejected by signer: {e}");
                last_signer_err = Some(e.to_string());
                continue;
            }
        }
    }

    if let Some(err) = last_signer_err {
        Err(Error::Agent(format!(
            "no agent identity was accepted by the server (last signer error: {err})"
        )))
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rsa_hash_only_for_rsa_keys() {
        assert!(rsa_hash_alg(&Algorithm::Ed25519, Some(HashAlg::Sha512)).is_none());
        assert_eq!(
            rsa_hash_alg(&Algorithm::Rsa { hash: None }, Some(HashAlg::Sha512)),
            Some(HashAlg::Sha512)
        );
    }

    #[tokio::test]
    async fn connect_without_agent_errors_clearly() {
        // Force a missing socket without disturbing the real environment
        // for other tests in this process: spawn a subprocess? For unit
        // scope we just map the known error variants.
        let err = map_agent_connect_error(russh::keys::Error::EnvVar("SSH_AUTH_SOCK"));
        assert!(matches!(err, Error::Agent(msg) if msg.contains("SSH_AUTH_SOCK")));
        let err = map_agent_connect_error(russh::keys::Error::BadAuthSock);
        assert!(matches!(err, Error::Agent(msg) if msg.contains("missing socket")));
    }
}
