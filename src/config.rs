use anyhow::Result;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub redis_url: Option<String>,
    pub listen: String,
    pub h2c: bool,
    /// Opt-in: use the native `crates/xlite-h1-pool` connection pool for
    /// cleartext `http://` upstreams instead of reqwest + `AdmissionGate`. Off by
    /// default (experimental; see `docs/dev/h1-upstream-pool-architecture.md`).
    pub h1_pool: bool,
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
        let h1_pool = env_flag("XLITE_UPSTREAM_H1_POOL");
        let admin_password =
            std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
        let session_secret = std::env::var("ADMIN_SESSION_SECRET")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let max_conns_per_host =
            positive_or(std::env::var("XLITE_UPSTREAM_MAX_CONNS_PER_HOST").ok(), 32);
        let upstream_stream_cap =
            positive_or(std::env::var("XLITE_UPSTREAM_STREAM_CAP").ok(), usize::MAX);
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
            h1_pool,
            admin_password,
            session_secret,
            max_conns_per_host,
            upstream_stream_cap,
            catalog_reload_secs,
            upstream_ttft_header,
        })
    }

    /// Whether the opt-in h1 pool is actually in effect. `H2C=1` owns cleartext
    /// `http://` outright, so a set-but-shadowed `XLITE_UPSTREAM_H1_POOL` must not
    /// be reported as active — in that case the pool is not even built.
    pub fn h1_pool_active(&self) -> bool {
        self.h1_pool && !self.h2c
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

/// The process's **soft** `RLIMIT_NOFILE`, read from `/proc/self/limits`.
///
/// Read from procfs rather than `libc::getrlimit` to avoid pulling in a
/// dependency; on non-Linux targets this reports `None` (the gateway only runs
/// on Linux in production — macOS is a development machine).
pub fn nofile_soft_limit() -> Option<u64> {
    parse_nofile(&proc_self_limits()?)
}

/// `/proc/self/limits` on Linux; nothing to read elsewhere.
fn proc_self_limits() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/limits").ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Pick the soft column out of the `Max open files` line of `/proc/self/limits`,
/// which looks like this — the columns are separated by a *run* of spaces and
/// either limit may be the literal `unlimited`:
///
/// ```text
/// Max open files            1024                 1048576              files
/// ```
fn parse_nofile(contents: &str) -> Option<u64> {
    contents
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .and_then(|line| line.split_whitespace().nth(3)) // "Max" "open" "files" <soft>
        .and_then(|soft| soft.parse().ok())
}

/// A comfortable fd budget for a gateway that may face tens of thousands of
/// concurrent clients: every inbound connection, every pooled upstream
/// connection, HAProxy's health checks, logs and sockets all come out of the one
/// process limit.
const FD_COMFORTABLE: u64 = 65_536;

/// Startup self-check for the fd budget: the warning to log, or `None`.
///
/// The failure mode this catches is quiet, which is what makes it expensive: with
/// `RLIMIT_NOFILE` clamped to the Docker default of 1024 the gateway does not
/// error — it forwards mass **502s at a tenth of the throughput** while `fail`,
/// `vus` and `iteration_duration` all look healthy. Measured 2026-09-30 (issue
/// #49): 22.78M `PoolError::Dial` failures and 99.49% of requests 502 with the
/// limit at 1024, versus 0.00% and the same 1,484 qps once it was raised.
pub fn fd_budget_warning(nofile: u64, max_conns_per_host: usize) -> Option<String> {
    if nofile >= FD_COMFORTABLE {
        return None;
    }
    Some(format!(
        "RLIMIT_NOFILE soft limit is {nofile} (< {FD_COMFORTABLE}). At ≥10k concurrent \
         clients a gateway can run out of file descriptors long before it runs out of CPU, \
         and the signature is a flood of 502s with a ~10x throughput drop while every other \
         metric still looks normal (see issue #49). On top of one fd per inbound request, \
         each upstream host may hold up to MAX_CONNS_PER_HOST={max_conns_per_host} pooled \
         connections. Raise it — e.g. `--ulimit nofile=1048576:1048576` for docker."
    ))
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

    #[test]
    fn parse_nofile_reads_the_soft_column() {
        let proc_limits = "Limit                     Soft Limit           Hard Limit           Units\n\
                            Max cpu time              unlimited            unlimited            seconds\n\
                            Max open files            1024                 1048576              files\n";
        assert_eq!(parse_nofile(proc_limits), Some(1024));
        // A soft limit of `unlimited` parses to None, i.e. "nothing to warn
        // about" — the check only ever fires on a number it can compare.
        let unlimited =
            "Max open files            unlimited            unlimited            files\n";
        assert_eq!(parse_nofile(unlimited), None);
        assert_eq!(parse_nofile("Limit   Soft Limit\n"), None);
    }

    #[test]
    fn fd_budget_warning_flags_a_container_default() {
        let warning = fd_budget_warning(1024, 32).expect("1024 is the trap this check exists for");
        assert!(warning.contains("1024"), "{warning}");
        assert!(warning.contains("MAX_CONNS_PER_HOST=32"), "{warning}");
        assert!(
            warning.contains("#49"),
            "the warning has to carry the measurement that justifies it: {warning}"
        );
    }

    #[test]
    fn fd_budget_warning_is_quiet_when_the_limit_is_raised() {
        assert!(fd_budget_warning(FD_COMFORTABLE, 32).is_none());
        assert!(fd_budget_warning(1_048_576, 32).is_none());
    }
}
