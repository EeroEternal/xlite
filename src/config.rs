use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub listen: String,
    pub h2c: bool,
    pub admin_password: String,
    pub max_conns_per_host: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
        let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
        let host = std::env::var("LISTEN_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let h2c = env_flag("XLITE_UPSTREAM_H2C");
        let admin_password =
            std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
        let max_conns_per_host = std::env::var("XLITE_UPSTREAM_MAX_CONNS_PER_HOST")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n: &usize| n > 0)
            .unwrap_or(32);
        Ok(Self {
            database_url,
            listen: format!("{host}:{port}"),
            h2c,
            admin_password,
            max_conns_per_host,
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
}
