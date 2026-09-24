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
    /// 0 = unlimited. On a 1 GbE path, cap near ~pipe (e.g. 8000) so
    /// 31k VUs do not slice the NIC into 30s streams.
    pub max_in_flight: usize,
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
        let max_conns_per_host = std::env::var("XLITE_UPSTREAM_MAX_CONNS_PER_HOST")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(32);
        let max_in_flight = std::env::var("XLITE_MAX_IN_FLIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Ok(Self {
            database_url,
            redis_url,
            listen: format!("{host}:{port}"),
            h2c,
            admin_password,
            session_secret,
            max_conns_per_host,
            max_in_flight,
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
    fn memory_urls() {
        assert!(is_memory_url("memory"));
        assert!(is_memory_url("sqlite::memory:"));
        assert!(!is_memory_url("sqlite:xlite.db"));
    }
}
