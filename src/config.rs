use anyhow::Result;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub redis_url: Option<String>,
    pub listen: String,
    pub h2c: bool,
    pub admin_password: String,
    pub session_secret: Option<String>,
    pub max_conns_per_host: usize,
    /// Client-side cap on concurrent streams per upstream HTTP/2 connection:
    /// a connection is full at `min(this, the peer's live
    /// SETTINGS_MAX_CONCURRENT_STREAMS)`. `usize::MAX` (the default, from an
    /// unset/invalid/zero env) means "let the peer decide" — the Go
    /// `mock-llm:h2c` farm advertises the Go-library default 250 on every
    /// connection, so per-mock admission capacity is
    /// `max_conns_per_host × 250` unless this is lowered. Lowering it turns the
    /// pool into a transport-level admission gate: excess streams queue at the
    /// pool (wait lands in TTFT, never an error) while the pool dials more
    /// connections, up to `max_conns_per_host`.
    pub upstream_stream_cap: usize,
    /// Catalog snapshot reload period. `0` disables the background loop (load test).
    pub catalog_reload_secs: u64,
    /// Diagnostic-only: stamp `x-xlite-upstream-ttft-ms` (time from sending the
    /// upstream request to its first body byte) on streaming responses. Off by
    /// default, and never used for the V88 comparison table: New API emits no
    /// equivalent header, so the column would be one-sided.
    pub upstream_ttft_header: bool,
}

/// Parse a positive-integer env value. `None`/invalid/`0` fall back to
/// `default` — the silent-fallback shape rig scripts guard against by
/// validating values up front (`scripts/loadtest/ab_configs.sh`).
fn positive_or(raw: Option<String>, default: usize) -> usize {
    raw.and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let redis_url = std::env::var("REDIS_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:xlite.db".into());
        let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
        let host = std::env::var("LISTEN_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let h2c = env_flag("XLITE_UPSTREAM_H2C");
        let admin_password =
            std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
        let session_secret = std::env::var("ADMIN_SESSION_SECRET")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let max_conns_per_host = positive_or(
            std::env::var("XLITE_UPSTREAM_MAX_CONNS_PER_HOST").ok(),
            32,
        );
        let upstream_stream_cap = positive_or(
            std::env::var("XLITE_UPSTREAM_STREAM_CAP").ok(),
            usize::MAX,
        );
        let catalog_reload_secs = std::env::var("XLITE_CATALOG_RELOAD_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let upstream_ttft_header = env_flag("XLITE_UPSTREAM_TTFT_HEADER");
        Ok(Self {
            database_url,
            redis_url,
            listen: format!("{host}:{port}"),
            h2c,
            admin_password,
            session_secret,
            max_conns_per_host,
            upstream_stream_cap,
            catalog_reload_secs,
            upstream_ttft_header,
        })
    }
}

pub fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| parse_flag(&value))
        .unwrap_or(false)
}

fn parse_flag(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn is_memory_url(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u == "memory"
        || u == ":memory:"
        || u == "sqlite::memory:"
        || u == "sqlite://:memory:"
        || u.contains("mode=memory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_flag_accepts_common_truthy() {
        assert!(parse_flag("1"));
        assert!(parse_flag("TRUE"));
        assert!(parse_flag(" yes "));
        assert!(!parse_flag("0"));
        assert!(!parse_flag("false"));
    }

    #[test]
    fn stream_cap_defaults_to_peer_decides() {
        // Unset / invalid / 0 all mean "let the peer decide" (usize::MAX):
        // the pool then executes the peer's advertised
        // SETTINGS_MAX_CONCURRENT_STREAMS as-is.
        assert_eq!(positive_or(None, usize::MAX), usize::MAX);
        assert_eq!(positive_or(Some("0".into()), usize::MAX), usize::MAX);
        assert_eq!(positive_or(Some("abc".into()), usize::MAX), usize::MAX);
        assert_eq!(positive_or(Some("40".into()), usize::MAX), 40);
        assert_eq!(positive_or(Some("-1".into()), usize::MAX), usize::MAX);
        // Same helper backs MAX_CONNS_PER_HOST's default of 32.
        assert_eq!(positive_or(None, 32), 32);
    }

    #[test]
    fn memory_urls() {
        assert!(is_memory_url("memory"));
        assert!(is_memory_url("sqlite::memory:"));
        assert!(!is_memory_url("sqlite:xlite.db"));
    }
}
