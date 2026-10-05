//! A domain's DKIM signing key, created by `provision-apply` where Rspamd's `dkim_signing`
//! module reads it: `rsa-2048-<selector>-<domain>.private.txt` in docker-mailserver's
//! `rspamd/dkim/` directory, the same name docker-mailserver's own `rspamd-dkim` command uses.
//!
//! An existing key there is adopted, never replaced, because the domain's sending reputation is
//! attached to the published key. A new one is RSA-2048 from `aws-lc-rs`, written as PKCS#8 PEM
//! with mode 0600 and the directory's owner, which the deployment sets to Rspamd's user so
//! Rspamd can read it; PKCS#1 keys (`RSA PRIVATE KEY`) are read too. Only the public half, the
//! SubjectPublicKeyInfo in base64, leaves this module: it becomes the `p=` of the
//! `<selector>._domainkey.<domain>` TXT record (RFC 6376). If Rspamd cannot read a key, its
//! signing fails and the sender policy defers the mail rather than send it unsigned.

use std::path::Path;
use std::{fs, io};

use aws_lc_rs::encoding::{AsDer as _, Pkcs8V1Der, PublicKeyX509Der};
use aws_lc_rs::rsa::{KeyPair, KeySize};
use aws_lc_rs::signature::KeyPair as _;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use super::publish;

/// Why a key could not be provided.
#[derive(Debug, thiserror::Error)]
pub enum DkimError {
    /// The directory or the file cannot be used now; the change stays pending.
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The key file holds something that is not an RSA private key; the change is refused.
    #[error("{0}")]
    Key(String),
}

/// The base64 public key of `domain`'s key for `selector`, creating the key when none exists.
///
/// # Errors
///
/// The directory is missing or unwritable, or an existing file is not an RSA private key.
pub fn ensure(dir: &Path, domain: &str, selector: &str) -> Result<String, DkimError> {
    if !dir.is_dir() {
        return Err(DkimError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "{} is missing; the deployment creates it owned by Rspamd's user",
                dir.display()
            ),
        )));
    }
    let path = dir.join(format!("rsa-2048-{selector}-{domain}.private.txt"));
    let pair = match fs::read_to_string(&path) {
        Ok(pem) => parse(&pem).ok_or_else(|| {
            DkimError::Key(format!("{} is not an RSA private key", path.display()))
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let pair = KeyPair::generate(KeySize::Rsa2048)
                .map_err(|_| DkimError::Key("RSA key generation failed".to_owned()))?;
            let der: Pkcs8V1Der<'static> = pair
                .as_der()
                .map_err(|_| DkimError::Key("the new key cannot be encoded".to_owned()))?;
            publish::publish(&path, pem("PRIVATE KEY", der.as_ref()).as_bytes(), 0o600)?;
            tracing::info!(domain, selector, "DKIM key created");
            pair
        }
        Err(error) => return Err(DkimError::Io(error)),
    };
    let public: PublicKeyX509Der<'static> = pair
        .public_key()
        .as_der()
        .map_err(|_| DkimError::Key("the public key cannot be encoded".to_owned()))?;
    Ok(STANDARD.encode(public.as_ref()))
}

/// An RSA key from PEM: PKCS#8 (`PRIVATE KEY`) or PKCS#1 (`RSA PRIVATE KEY`).
fn parse(text: &str) -> Option<KeyPair> {
    let mut label = None;
    let mut body = String::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|l| l.strip_suffix("-----"))
        {
            label = Some(rest.to_owned());
        } else if line.starts_with("-----END ") {
            break;
        } else if label.is_some() {
            body.push_str(line);
        }
    }
    let der = STANDARD.decode(body).ok()?;
    match label.as_deref()? {
        "PRIVATE KEY" => KeyPair::from_pkcs8(&der).ok(),
        "RSA PRIVATE KEY" => KeyPair::from_der(&der).ok(),
        _ => None,
    }
}

/// PEM with 64-character lines.
fn pem(label: &str, der: &[u8]) -> String {
    let encoded = STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    let mut rest = encoded.as_str();
    while !rest.is_empty() {
        let Some((line, tail)) = rest.split_at_checked(rest.len().min(64)) else {
            break;
        };
        out.push_str(line);
        out.push('\n');
        rest = tail;
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::testing::TempDir;

    /// A missing directory is an I/O failure that leaves the change pending; the first call
    /// creates an owner-only PKCS#8 key and returns its public half; later calls adopt that key
    /// rather than replace it, because the published key carries the domain's reputation.
    #[test]
    fn creates_once_then_adopts() {
        let dir = TempDir::new();
        let keys = dir.join("dkim");
        assert!(matches!(
            ensure(&keys, "example.com", "norbelys"),
            Err(DkimError::Io(_))
        ));
        fs::create_dir(&keys).unwrap();
        let public = ensure(&keys, "example.com", "norbelys").unwrap();
        let path = keys.join("rsa-2048-norbelys-example.com.private.txt");
        let pem = fs::read_to_string(&path).unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(STANDARD.decode(&public).unwrap().len() > 256);
        assert_eq!(ensure(&keys, "example.com", "norbelys").unwrap(), public);
        assert_eq!(fs::read_to_string(&path).unwrap(), pem);
    }

    /// A file that holds no RSA key is refused, not overwritten: an operator decides.
    #[test]
    fn refuses_a_file_that_is_not_a_key() {
        let dir = TempDir::new();
        let path = dir.join("rsa-2048-norbelys-example.com.private.txt");
        fs::write(
            &path,
            "-----BEGIN PRIVATE KEY-----\nbm90IGEga2V5\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        assert!(matches!(
            ensure(dir.path(), "example.com", "norbelys"),
            Err(DkimError::Key(_))
        ));
        assert!(fs::read_to_string(&path).unwrap().contains("bm90IGEga2V5"));
    }
}
