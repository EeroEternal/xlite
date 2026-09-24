use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{get, post};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use xlite::admin::{self, AppState};
use xlite::auth::Sessions;
use xlite::catalog::Catalog;
use xlite::catalog::yaml::{self, ImportMode};
use xlite::config::Config;
use xlite::proxy;
use xlite::store::Store;
use xlite::upstream::Upstream;

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("import") => {
            init_logging();
            let path = args.next().context("usage: xlite import <file.yaml>")?;
            cmd_import(&path, args.next().as_deref()).await
        }
        Some("export") => {
            init_logging();
            cmd_export(args.next().as_deref()).await
        }
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command {other}; try xlite --help"),
        None => run_server().await,
    }
}

fn init_logging() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xlite=info,tower_http=info,sqlx=warn,redis=warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn print_help() {
    eprintln!(
        "xlite — thin OpenAI gateway\n\n\
         xlite                      start the server\n\
         xlite import <file.yaml>   merge catalog YAML (add --replace to wipe first)\n\
         xlite export [file.yaml]   write catalog YAML to a file or stdout"
    );
}

async fn cmd_import(path: &str, extra: Option<&str>) -> Result<()> {
    let replace = extra == Some("--replace") || extra == Some("replace");
    let text = std::fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let config = Config::from_env()?;
    let catalog = Catalog::connect(&config).await.context("open catalog")?;
    let mode = if replace {
        ImportMode::Replace
    } else {
        ImportMode::Merge
    };
    let report = yaml::import_yaml(&catalog, &text, mode).await?;
    println!(
        "imported providers +{}/~{} models +{}/~{} channels +{}/~{} keys +{}/~{}",
        report.providers.created,
        report.providers.updated,
        report.models.created,
        report.models.updated,
        report.channels.created,
        report.channels.updated,
        report.api_keys.created,
        report.api_keys.updated
    );
    for secret in report.secrets {
        println!("api key {}: {}", secret.name, secret.key);
    }
    Ok(())
}

async fn cmd_export(path: Option<&str>) -> Result<()> {
    let config = Config::from_env()?;
    let catalog = Catalog::connect(&config).await.context("open catalog")?;
    let text = yaml::export_yaml(&catalog).await?;
    if let Some(path) = path {
        std::fs::write(path, &text).with_context(|| format!("write {path}"))?;
    } else {
        print!("{text}");
    }
    Ok(())
}

async fn run_server() -> Result<()> {
    init_logging();
    let config = Config::from_env()?;
    info!(
        gateway = "xlite",
        version = env!("CARGO_PKG_VERSION"),
        listen = %config.listen,
        h2c = config.h2c,
        redis = config.redis_url.is_some(),
        database = %config.database_url,
        max_conns = config.max_conns_per_host,
        "starting"
    );
    if config.admin_password == "admin" {
        tracing::warn!("ADMIN_PASSWORD is the default; set it before any shared deployment");
    }

    let catalog = Catalog::connect(&config).await.context("open catalog")?;
    let store = Store::new(catalog);
    store.reload().await.context("load routing snapshot")?;
    let upstream = Upstream::new(&config);
    let state = AppState {
        store: store.clone(),
        sessions: Sessions::default(),
        admin_password: config.admin_password.clone(),
        upstream,
        config: config.clone(),
    };

    let reload_store = store.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Err(err) = reload_store.reload().await {
                tracing::warn!("snapshot reload failed: {err}");
            }
        }
    });

    let mut app = Router::new()
        .route("/health", get(proxy::health))
        .route("/ready", get(proxy::ready))
        .route("/v1/models", get(proxy::list_models))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .merge(admin::router())
        .layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::predicate(|origin, _| {
                    origin.as_bytes().starts_with(b"http://localhost")
                        || origin.as_bytes().starts_with(b"http://127.0.0.1")
                }))
                .allow_credentials(true)
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                    axum::http::header::COOKIE,
                ])
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PUT,
                    axum::http::Method::DELETE,
                    axum::http::Method::OPTIONS,
                ]),
        )
        .with_state(state);

    let dist = PathBuf::from("admin/dist");
    if dist.join("index.html").exists() {
        let index = ServeFile::new(dist.join("index.html"));
        app = app.fallback_service(ServeDir::new(dist).not_found_service(index));
        info!("serving admin UI from admin/dist");
    }

    let addr: SocketAddr = config.listen.parse().context("parse listen address")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    info!("listening on {addr}");
    axum::serve(listener, app.into_make_service())
        .await
        .context("serve")?;
    Ok(())
}
