//! The long-lived credential: refresh token and stable agent id, kept in a private file.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use reverse_core::ids::AgentId;
use reverse_core::secret::SecretString;
use serde::{Deserialize, Serialize};

/// What survives a restart: who this agent is and how to renew its access token.
pub struct Credential {
    /// Stable id of this agent instance.
    pub agent_id: AgentId,
    /// Rotating refresh token.
    pub refresh_token: SecretString,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileCredential {
    agent_id: String,
    refresh_token: String,
}

/// Reads the saved credential; `None` when the agent never enrolled.
pub fn load(path: &Path) -> io::Result<Option<Credential>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let saved: FileCredential = serde_json::from_str(&raw)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let agent_id = AgentId::parse(&saved.agent_id)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    if saved.refresh_token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty refresh token",
        ));
    }
    Ok(Some(Credential {
        agent_id,
        refresh_token: SecretString::new(saved.refresh_token),
    }))
}

/// Saves the credential readable by the owner only.
pub fn save(path: &Path, credential: &Credential) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let saved = FileCredential {
        agent_id: credential.agent_id.as_str().to_owned(),
        refresh_token: credential.refresh_token.expose().to_owned(),
    };
    let bytes = serde_json::to_vec(&saved).map_err(io::Error::other)?;
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Deletes a credential the identity provider no longer accepts.
pub fn forget(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "credential.forget_failed");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_saved_credential_is_private_and_readable_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("credential");
        assert!(load(&path).unwrap().is_none());
        let credential = Credential {
            agent_id: AgentId::parse("0123456789abcdef").unwrap(),
            refresh_token: SecretString::new("refresh-abc".into()),
        };
        save(&path, &credential).unwrap();
        let back = load(&path).unwrap().unwrap();
        assert_eq!(back.agent_id.as_str(), "0123456789abcdef");
        assert_eq!(back.refresh_token.expose(), "refresh-abc");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        forget(&path);
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_err());
    }
}
