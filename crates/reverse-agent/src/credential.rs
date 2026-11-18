//! The credential obtained by pairing, kept in a private file between restarts.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use reverse_core::secret::SecretString;

/// Reads the saved credential; `None` when the agent never paired.
pub fn load(path: &Path) -> io::Result<Option<SecretString>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let token = raw.trim();
            Ok((!token.is_empty()).then(|| SecretString::new(token.to_owned())))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Saves the credential readable by the owner only.
pub fn save(path: &Path, token: &SecretString) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(token.expose().as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Deletes a credential the relay no longer accepts.
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
        save(&path, &SecretString::new("rvr_abc".into())).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().expose(), "rvr_abc");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        forget(&path);
        assert!(load(&path).unwrap().is_none());
    }
}
