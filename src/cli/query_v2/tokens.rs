//! Local authenticated capabilities. These are not portable evidence identities.
use std::fs;
use std::io::Write;

use hmac::{Hmac, Mac};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use super::super::{Result, error, io_error, new_file, read_limited, regular};

pub(super) fn hash(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(invalid());
    }
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(invalid()),
            };
            Ok((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

fn invalid() -> super::super::Error {
    error("invalid_token", "invalid or unauthenticated v2 token", 3)
}

pub(super) struct Signer([u8; 32]);

impl Signer {
    pub fn load() -> Result<Self> {
        let directory = key_directory().ok_or_else(|| {
            error(
                "token_key_unavailable",
                "no user configuration directory",
                5,
            )
        })?;
        fs::create_dir_all(&directory).map_err(io_error)?;
        let path = directory.join("query-v2.key");
        if !path.try_exists().map_err(io_error)? {
            // Publish only a complete key. Concurrent first callers may race;
            // exactly one hard link wins and all callers read the winning key.
            let temporary = directory.join(format!(".query-v2-{}.key", uuid::Uuid::new_v4()));
            let mut bytes = [0u8; 32];
            bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            let result = (|| {
                let mut file = new_file(&temporary)?;
                file.write_all(&bytes).map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                drop(file);
                match fs::hard_link(&temporary, &path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(e) => Err(io_error(e)),
                }
            })();
            let _ = fs::remove_file(&temporary);
            result?;
        }
        regular(&path)?;
        let metadata = fs::metadata(&path).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(error(
                    "token_key_unavailable",
                    "v2 token key must be private (mode 0600)",
                    5,
                ));
            }
        }
        if metadata.len() != 32 {
            return Err(error(
                "token_key_unavailable",
                "invalid v2 token key length",
                5,
            ));
        }
        let bytes: [u8; 32] = read_limited(&path, 32)?
            .try_into()
            .map_err(|_| error("token_key_unavailable", "invalid v2 token key length", 5))?;
        Ok(Self(bytes))
    }

    pub fn seal(&self, kind: &str, value: &impl Serialize) -> Result<String> {
        let bytes = serde_json::to_vec(value).map_err(io_error)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("valid HMAC key");
        mac.update(kind.as_bytes());
        mac.update(&bytes);
        Ok(format!(
            "{kind}.{}.{}",
            hex(&bytes),
            hex(&mac.finalize().into_bytes())
        ))
    }

    pub fn open<T: DeserializeOwned>(&self, kind: &str, token: &str) -> Result<T> {
        if token.len() > 32768 {
            return Err(invalid());
        }
        let mut parts = token.split('.');
        if parts.next() != Some(kind) {
            return Err(invalid());
        }
        let bytes = unhex(parts.next().ok_or_else(invalid)?)?;
        let signature = unhex(parts.next().ok_or_else(invalid)?)?;
        if parts.next().is_some() {
            return Err(invalid());
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("valid HMAC key");
        mac.update(kind.as_bytes());
        mac.update(&bytes);
        mac.verify_slice(&signature).map_err(|_| invalid())?;
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }
}

// Keep the same platform locations as the GUI's config::config_path. That
// module belongs to the GUI binary and is not a headless library dependency.
fn key_directory() -> Option<std::path::PathBuf> {
    use std::{env, path::PathBuf};
    if let Some(path) = env::var_os("CIALLOGCAT_CONFIG_DIR") {
        return Some(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        env::var_os("APPDATA").map(|path| PathBuf::from(path).join("Ciallogcat"))
    }
    #[cfg(not(windows))]
    {
        env::var_os("XDG_CONFIG_HOME")
            .map(|path| PathBuf::from(path).join("ciallogcat"))
            .or_else(|| {
                env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/ciallogcat"))
            })
    }
}
