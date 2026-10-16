use std::time::Duration;

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
    /// Per-host connection cap for the native cleartext HTTP/1.1 pool.
    ///
    /// Deliberately **separate** from `max_conns_per_host`: that knob's default of
    /// 32 is tuned for the h2c pool, where one connection carries many
    /// multiplexed streams (`upstream_stream_cap`). Under HTTP/1.1 one connection
    /// carries **one** request, so 32 means "32 concurrent" (raising `32 → 128`
    /// under cleartext HTTP/1.1 yields ~3.7× QPS). When the operator sets
    /// `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` explicitly the h1 pool honours it
    /// verbatim; when it is unset, h1 falls back to
    /// [`H1_DEFAULT_MAX_CONNS_PER_HOST`] instead of the h2c default.
    pub h1_max_conns_per_host: usize,
    /// Client-side cap on concurrent streams per upstream HTTP/2 connection:
    /// a connection is full at `min(this, the peer's live
    /// SETTINGS_MAX_CONCURRENT_STREAMS)`. `usize::MAX` (the default, from an
    /// unset/invalid/zero env) means "let the peer decide". Lowering it turns the
    /// pool into a transport-level admission gate: excess streams queue at the
    /// pool (wait lands in TTFT, never an error) while the pool dials more
    /// connections, up to `max_conns_per_host`.
    pub upstream_stream_cap: usize,
    /// Catalog snapshot reload period. `0` disables the background loop (load test).
    pub catalog_reload_secs: u64,
    /// Diagnostic-only: stamp `x-xlite-upstream-ttft-ms` (time from sending the
    /// upstream request to its first body byte) on streaming responses. Off by
    /// default.
    pub upstream_ttft_header: bool,
    /// End an upstream response body with an error when it has produced no bytes
    /// for this long. `Duration::ZERO` (the default, from an unset / invalid / 0
    /// env) keeps the pre-existing behaviour: wait as long as the upstream does.
    ///
    /// This is a **liveness guard, not a throughput knob**: it bounds how long a
    /// hung upstream can park a gateway stream (and its pool lease), at the cost
    /// of turning the hang into an error. It must therefore be set above the
    /// longest legitimate upstream silence (a reasoning model can pause tens of
    /// seconds), which is why it is off unless an operator opts in.
    pub upstream_stall_timeout: Duration,
    /// Give up when the upstream has not returned **response headers** within
    /// this long after the request was sent. `Duration::ZERO` (the default)
    /// disables it.
    ///
    /// This is the header-phase counterpart of
    /// [`Config::upstream_stall_timeout`], for an upstream that accepts a stream
    /// and then never answers. It deliberately bounds only the request→headers
    /// wait: the dial and the pool-queue wait are **not** covered, so legitimate
    /// backpressure under load is not turned into errors. Because header timeouts
    /// happen before any client byte is written, `try_channel` fails over to the
    /// next candidate channel; if all `N` candidates hang on headers, the worst-case
    /// wait before a final `502` scales to `N × upstream_header_timeout`.
    pub upstream_header_timeout: Duration,
}

/// Parse a `*_SECS` duration env value: a positive integer number of seconds, or
/// [`Duration::ZERO`] (disabled) for unset / invalid / `0`.
fn timeout_secs_or_zero(raw: Option<&str>) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::ZERO)
}

/// Parse a positive-integer env value. `None`/invalid/`0` fall back to
/// `default` — the silent-fallback shape rig scripts guard against by
/// validating values up front (`scripts/loadtest/ab_configs.sh`).
fn positive_or(raw: Option<String>, default: usize) -> usize {
    raw.and_then(|v| v.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(default)
}

/// Default per-host connection cap for the cleartext HTTP/1.1 pool when
/// `XLITE_UPSTREAM_MAX_CONNS_PER_HOST` is unset. See
/// [`Config::h1_max_conns_per_host`]: it is 4× the h2c default of 32 because
/// HTTP/1.1 spends one connection per in-flight request, not per many streams.
pub const H1_DEFAULT_MAX_CONNS_PER_HOST: usize = 128;

/// Resolve the h1 pool cap: an explicit, valid `XLITE_UPSTREAM_MAX_CONNS_PER_HOST`
/// wins (the operator named a number — honour it verbatim, including a
/// lowering). Anything else — unset, empty, non-numeric or `0` — means "not
/// configured", so h1 keeps its own default instead of inheriting the h2c-tuned
/// one. `positive_or` folds that same string into the shared `max_conns_per_host`
/// default, so the two parsers agree on what "explicit" means.
fn resolve_h1_max_conns_per_host(explicit: Option<&str>) -> usize {
    explicit
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(H1_DEFAULT_MAX_CONNS_PER_HOST)
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
        let max_conns_env = std::env::var("XLITE_UPSTREAM_MAX_CONNS_PER_HOST").ok();
        let max_conns_per_host = positive_or(max_conns_env.clone(), 32);
        let h1_max_conns_per_host = resolve_h1_max_conns_per_host(max_conns_env.as_deref());
        let upstream_stream_cap =
            positive_or(std::env::var("XLITE_UPSTREAM_STREAM_CAP").ok(), usize::MAX);
        let catalog_reload_secs = std::env::var("XLITE_CATALOG_RELOAD_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let upstream_ttft_header = env_flag("XLITE_UPSTREAM_TTFT_HEADER");
        let upstream_stall_timeout = timeout_secs_or_zero(
            std::env::var("XLITE_UPSTREAM_STALL_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        );
        let upstream_header_timeout = timeout_secs_or_zero(
            std::env::var("XLITE_UPSTREAM_HEADER_TIMEOUT_SECS")
                .ok()
                .as_deref(),
        );
        Ok(Self {
            database_url,
            redis_url,
            listen: format!("{host}:{port}"),
            h2c,
            admin_password,
            session_secret,
            max_conns_per_host,
            h1_max_conns_per_host,
            upstream_stream_cap,
            catalog_reload_secs,
            upstream_ttft_header,
            upstream_stall_timeout,
            upstream_header_timeout,
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
pub fn fd_budget_warning(nofile: u64, pooled_conns_per_host: usize) -> Option<String> {
    if nofile >= FD_COMFORTABLE {
        return None;
    }
    Some(format!(
        "RLIMIT_NOFILE soft limit is {nofile} (< {FD_COMFORTABLE}). At ≥10k concurrent \
         clients a gateway can run out of file descriptors long before it runs out of CPU, \
         and the signature is a flood of 502s with a ~10x throughput drop while every other \
         metric still looks normal (see issue #49). On top of one fd per inbound request, \
         each upstream host may hold up to {pooled_conns_per_host} pooled connections (the \
         larger of the h2c and HTTP/1.1 per-host caps). Raise it — e.g. \
         `--ulimit nofile=1048576:1048576` for docker."
    ))
}

/// Startup self-check for legacy `XROUTER_*` variables, common `XLITE_*` typos,
/// and unsupported Postgres `DATABASE_URL` values carried over from older
/// `xrouter`/`xgateway` launch scripts.
pub fn legacy_env_warnings_from_env() -> Vec<String> {
    let vars: Vec<(String, String)> = std::env::vars().collect();
    legacy_env_warnings(vars.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

pub fn legacy_env_warnings<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    let mut xrouter_keys = Vec::new();
    let mut has_conns_typo = false;
    let mut has_max_conns = false;
    let mut database_url: Option<&'a str> = None;
    let mut has_redis_url = false;

    for (k, v) in vars {
        if k.starts_with("XROUTER_") {
            xrouter_keys.push(k);
        }
        if k == "XLITE_UPSTREAM_CONNS_PER_HOST" {
            has_conns_typo = true;
        }
        if k == "XLITE_UPSTREAM_MAX_CONNS_PER_HOST" {
            has_max_conns = true;
        }
        if k == "DATABASE_URL" && !v.trim().is_empty() {
            database_url = Some(v.trim());
        }
        if k == "REDIS_URL" && !v.trim().is_empty() {
            has_redis_url = true;
        }
    }

    xrouter_keys.sort_unstable();
    xrouter_keys.dedup();

    let mut warnings = Vec::new();
    if !xrouter_keys.is_empty() {
        warnings.push(format!(
            "detected legacy XROUTER_* environment variable(s): {}. \
             xlite ignores all XROUTER_* variables; use XLITE_* instead \
             (e.g. XLITE_UPSTREAM_H2C, XLITE_UPSTREAM_MAX_CONNS_PER_HOST, \
             XLITE_UPSTREAM_STREAM_CAP, XLITE_CATALOG_RELOAD_SECS) and TOKIO_WORKER_THREADS.",
            xrouter_keys.join(", ")
        ));
    }
    if has_conns_typo && !has_max_conns {
        warnings.push(
            "XLITE_UPSTREAM_CONNS_PER_HOST is set, but the supported variable name is \
             XLITE_UPSTREAM_MAX_CONNS_PER_HOST (with `_MAX_`); the unset default was used."
                .to_string(),
        );
    }
    if let Some(db) = database_url {
        let lower = db.to_ascii_lowercase();
        if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
            if has_redis_url {
                warnings.push(
                    "DATABASE_URL points to PostgreSQL, which xlite does not support; \
                     it is ignored because REDIS_URL is set (xlite supports only REDIS_URL \
                     or SQLite/memory DATABASE_URL)."
                        .to_string(),
                );
            } else {
                warnings.push(
                    "DATABASE_URL points to PostgreSQL, which xlite does not support; \
                     set REDIS_URL or use a SQLite/memory DATABASE_URL (e.g. sqlite:xlite.db or memory)."
                        .to_string(),
                );
            }
        }
    }
    warnings
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
    fn timeout_secs_or_zero_is_off_unless_explicitly_set() {
        // Unset / invalid / 0 all mean "wait as long as the upstream does".
        assert_eq!(timeout_secs_or_zero(None), Duration::ZERO);
        assert_eq!(timeout_secs_or_zero(Some("0")), Duration::ZERO);
        assert_eq!(timeout_secs_or_zero(Some("abc")), Duration::ZERO);
        assert_eq!(timeout_secs_or_zero(Some("-5")), Duration::ZERO);
        // A positive value is honoured verbatim.
        assert_eq!(timeout_secs_or_zero(Some("120")), Duration::from_secs(120));
        assert_eq!(timeout_secs_or_zero(Some(" 90 ")), Duration::from_secs(90));
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
    fn h1_cap_honours_an_explicit_env_and_defaults_higher_otherwise() {
        // Unset / empty / invalid / 0 mean "not configured" ⇒ the h1-specific
        // default, not the h2c-tuned shared one (32).
        assert_eq!(resolve_h1_max_conns_per_host(None), 128);
        assert_eq!(resolve_h1_max_conns_per_host(Some("")), 128);
        assert_eq!(resolve_h1_max_conns_per_host(Some("abc")), 128);
        assert_eq!(resolve_h1_max_conns_per_host(Some("0")), 128);
        // An explicit, valid cap is honoured verbatim — including a lowering.
        assert_eq!(resolve_h1_max_conns_per_host(Some("128")), 128);
        assert_eq!(resolve_h1_max_conns_per_host(Some("4")), 4);
    }

    #[test]
    fn fd_budget_warning_flags_a_container_default() {
        let warning = fd_budget_warning(1024, 128).expect("1024 is the trap this check exists for");
        assert!(warning.contains("1024"), "{warning}");
        assert!(warning.contains("128 pooled connections"), "{warning}");
        assert!(
            warning.contains("#49"),
            "the warning has to carry the measurement that justifies it: {warning}"
        );
    }

    #[test]
    fn fd_budget_warning_is_quiet_when_the_limit_is_raised() {
        assert!(fd_budget_warning(FD_COMFORTABLE, 128).is_none());
        assert!(fd_budget_warning(1_048_576, 128).is_none());
    }

    #[test]
    fn legacy_env_warnings_catches_xrouter_and_postgres_footguns() {
        assert!(
            legacy_env_warnings([
                ("XLITE_UPSTREAM_H2C", "1"),
                ("XLITE_UPSTREAM_MAX_CONNS_PER_HOST", "64"),
                ("REDIS_URL", "redis://127.0.0.1:6379"),
            ])
            .is_empty()
        );

        let warns = legacy_env_warnings([
            ("XROUTER_UPSTREAM_CONNS_PER_HOST", "32"),
            ("XROUTER_UPSTREAM_H2C", "1"),
            ("XLITE_UPSTREAM_CONNS_PER_HOST", "64"),
            (
                "DATABASE_URL",
                "postgresql://xgateway:postgres@127.0.0.1:5432/xrouter",
            ),
            ("REDIS_URL", "redis://127.0.0.1:6379"),
        ]);
        assert_eq!(warns.len(), 3, "{warns:?}");
        assert!(
            warns[0].contains("XROUTER_UPSTREAM_CONNS_PER_HOST, XROUTER_UPSTREAM_H2C"),
            "{}",
            warns[0]
        );
        assert!(
            warns[1].contains("XLITE_UPSTREAM_MAX_CONNS_PER_HOST"),
            "{}",
            warns[1]
        );
        assert!(warns[2].contains("PostgreSQL"), "{}", warns[2]);
    }
}
