//! Opt-in TLS key logging for packet-level debugging.
//!
//! Off unless `XLITE_SSLKEYLOG` is set, so the normal path pays nothing: the rustls
//! `ClientConfig` keeps its default no-op key log. When enabled, secrets are written in
//! NSS key-log format so `tshark`/Wireshark can decrypt upstream TLS and show HTTP/2
//! frames (`RST_STREAM`, `HEADERS`, `DATA`) on the wire.
//!
//! Session secrets land on disk in plaintext. Debug and testing only — never enable it
//! on a long-running production node.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rustls::KeyLog;

/// Enables key logging: a file path, or `1`/`true` to reuse `SSLKEYLOGFILE`.
pub const ENV: &str = "XLITE_SSLKEYLOG";

/// Resolved output path, or `None` when key logging is off. Warns once when enabled.
pub fn path() -> Option<&'static PathBuf> {
    static RESOLVED: OnceLock<Option<PathBuf>> = OnceLock::new();
    RESOLVED
        .get_or_init(|| {
            let raw = std::env::var(ENV).ok()?.trim().to_owned();
            if raw.is_empty() {
                return None;
            }
            let path = if raw == "1" || raw.eq_ignore_ascii_case("true") {
                match std::env::var("SSLKEYLOGFILE") {
                    Ok(p) if !p.trim().is_empty() => PathBuf::from(p.trim()),
                    _ => {
                        tracing::warn!(
                            target: "xlite.security",
                            "{ENV}=1 but SSLKEYLOGFILE is unset; TLS key log disabled"
                        );
                        return None;
                    }
                }
            } else {
                PathBuf::from(raw)
            };
            tracing::warn!(
                target: "xlite.security",
                path = %path.display(),
                "TLS key log enabled: session secrets are written to disk (debug/testing only)"
            );
            Some(path)
        })
        .as_ref()
}

/// Appends NSS-format key log lines (`<label> <client_random> <secret>`) to a file.
#[derive(Debug)]
pub struct FileKeyLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl FileKeyLog {
    /// Installs key logging when `XLITE_SSLKEYLOG` is set.
    pub fn from_env() -> Option<Self> {
        path().map(|p| Self::with_path(p.clone()))
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
        }
    }
}

impl KeyLog for FileKeyLog {
    fn log(&self, label: &str, client_random: &[u8], secret: &[u8]) {
        let mut guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let file = match guard.as_mut() {
            Some(file) => file,
            None => match open(&self.path) {
                Ok(file) => guard.insert(file),
                Err(err) => {
                    tracing::warn!(
                        target: "xlite.security",
                        path = %self.path.display(),
                        %err,
                        "cannot open TLS key log"
                    );
                    return;
                }
            },
        };
        let mut line =
            String::with_capacity(1 + label.len() + 2 * (client_random.len() + secret.len()) + 2);
        line.push_str(label);
        for part in [client_random, secret] {
            line.push(' ');
            for byte in part {
                line.push(char::from(HEX[(byte >> 4) as usize]));
                line.push(char::from(HEX[(byte & 0x0f) as usize]));
            }
        }
        line.push('\n');
        if let Err(err) = file.write_all(line.as_bytes()) {
            tracing::warn!(target: "xlite.security", path = %self.path.display(), %err, "TLS key log write failed");
        }
    }
}

const HEX: [u8; 16] = *b"0123456789abcdef";

fn open(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    // Session secrets: keep the file owner-only (the default 0644 would expose them).
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_nss_format_lines() {
        let path = std::env::temp_dir().join(format!("xlite-keylog-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let log = FileKeyLog::with_path(path.clone());
        log.log("CLIENT_RANDOM", &[0x00, 0x00, 0xab, 0xff], &[0x0f, 0x10]);
        log.log("CLIENT_RANDOM", &[0x01], &[0x02]);
        let text = std::fs::read_to_string(&path).expect("key log written");
        assert_eq!(text, "CLIENT_RANDOM 0000abff 0f10\nCLIENT_RANDOM 01 02\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "key log must not be readable by other users");
        }
        let _ = std::fs::remove_file(&path);
    }
}
