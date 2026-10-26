//! The certificate for remote browsers. It is made on first run and kept in
//! the data directory, so a browser that accepted it keeps accepting it after
//! a restart. Nothing here is signed by an authority: a browser shows a
//! warning about it once, and the person using that browser accepts it.

use std::path::Path;

use anyhow::{Context, Result};

use crate::store::write_atomic;

/// The certificate and key as PEM, made on first run.
pub fn load_or_create(data_dir: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let dir = data_dir.join("tls");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read(&cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key =
            std::fs::read(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
        return Ok((cert, key));
    }

    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let made = rcgen::generate_simple_self_signed(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])
    .context("making the certificate for remote browsers")?;
    let cert = made.cert.pem().into_bytes();
    let key = made.key_pair.serialize_pem().into_bytes();
    write_atomic(&key_path, &key)?;
    restrict(&key_path)?;
    write_atomic(&cert_path, &cert)?;
    Ok((cert, key))
}

/// Only the sender's own user may read the key.
#[cfg(unix)]
fn restrict(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_certificate_is_kept_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(first, second);
        assert!(String::from_utf8_lossy(&first.0).contains("BEGIN CERTIFICATE"));
    }
}
