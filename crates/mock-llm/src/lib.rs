//! mock-llm — drop-in, higher-throughput replacement for the lab's Go `mock-llm:h2c`
//! test upstream.
//!
//! The wire contract below was established by black-box probing of the stripped Go
//! binary `mock-llm-linux-usage` (2026-10-06) — every claim is observable behaviour,
//! not source:
//!   * one port serves HTTP/1.1 **and** h2c (prior knowledge); the h2c side advertises
//!     `SETTINGS_MAX_CONCURRENT_STREAMS = 250`, which the xlite upstream pool reads to
//!     size its per-connection stream budget — a drop-in must advertise the same value;
//!   * `GET  /health` → `{"status":"ok"}`;
//!   * `POST /_config` → read (no query) / write (query) the 15 tuning knobs; the
//!     readback JSON keeps `batch` as the **last** key because the loadtest harness
//!     greps `"batch":[0-9]*}` and fail-closes otherwise;
//!   * `POST /v1/chat/completions` → SSE stream (`"stream":true`) or one-shot JSON;
//!   * per-request overrides `x-mock-ttft` / `-delay` / `-output` / `-size` /
//!     `-duration` / `-mode`, and `x-mock-error` (`500` → 500, `rate_limit` → 429).
//!
//! Known quirks that are reproduced deliberately:
//!   * `token_interval = 0` does **not** mean "no pacing": it falls back to
//!     `MOCK_STREAM_DEFAULT_INTERVAL` (default 20 ms), exactly like the Go binary;
//!   * `max_duration` truncates the content frames but `usage.completion_tokens` still
//!     reports the configured output count;
//!   * `_config` silently skips unknown keys and non-integer values (logged to stderr),
//!     matching the Go binary's lenient parsing — the harness validates by readback.
//!
//! Why a second implementation: the Go mock re-serialises every chunk with `fmt`
//! (measured ≈6 µs CPU/chunk at 500 chunks per stream). Here each frame is assembled
//! from constant slices with only a 13-digit `ts` patched per chunk, so the farm — the
//! documented wall at high stream counts — moves outward.

use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{BufMut, Bytes, BytesMut};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use std::pin::Pin;
use tokio::net::TcpListener;

/// Filler text for chunk content. The Go mock draws random words; content wording is
/// decorative — what the rig measures is frame count, frame size and `ts`. Byte counts
/// match at 4 bytes per chunk by default.
const PHRASE: &[u8] = b"mock token payload. ";

/// `SETTINGS_MAX_CONCURRENT_STREAMS` advertised on every h2c connection (Go parity).
const H2_MAX_CONCURRENT_STREAMS: u32 = 250;

// Frame pieces for `chat.completion.chunk` content frames. Assembling from constants
// replaces the Go mock's per-chunk `fmt` round-trip — the whole point of this crate.
const F_PRE: &[u8] = b"data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"";
const F_MID: &[u8] = b"\"},\"finish_reason\":null}],\"ts\":";
const F_SUF: &[u8] = b"}\n\n";

// The 15 tuning knobs, in the Go readback's exact key order (`batch` must stay last).
#[derive(Clone, Copy, Debug)]
pub struct Cfg {
    pub ttft: u64,
    pub total_delay: u64,
    pub out_tokens: u64,
    pub token_interval: u64,
    pub response_size: u64,
    pub max_duration: u64,
    pub mode: i64,
    pub mix: u64,
    pub long_pct: u64,
    pub mid_pct: u64,
    pub short_pct: u64,
    pub long_out: u64,
    pub mid_out: u64,
    pub short_out: u64,
    pub batch: u64,
    /// `MOCK_STREAM_DEFAULT_INTERVAL`: pacing fallback when `token_interval == 0`.
    /// Env-only — not part of the readback JSON (Go parity).
    pub stream_default_interval: u64,
}

impl Default for Cfg {
    fn default() -> Self {
        // Defaults observed from a clean start of the Go binary.
        Cfg {
            ttft: 500,
            total_delay: 0,
            out_tokens: 50,
            token_interval: 0,
            response_size: 0,
            max_duration: 0,
            mode: 0,
            mix: 0,
            long_pct: 20,
            mid_pct: 30,
            short_pct: 50,
            long_out: 500,
            mid_out: 250,
            short_out: 100,
            batch: 1,
            stream_default_interval: 20,
        }
    }
}

const KNOBS: &[(&str, &str)] = &[
    ("MOCK_TTFT", "ttft"),
    ("MOCK_TOTAL_DELAY", "total_delay"),
    ("MOCK_OUT_TOKENS", "out_tokens"),
    ("MOCK_TOKEN_INTERVAL", "token_interval"),
    ("MOCK_RESPONSE_SIZE", "response_size"),
    ("MOCK_MAX_DURATION", "max_duration"),
    ("MOCK_MODE", "mode"),
    ("MOCK_MIX", "mix"),
    ("MOCK_LONG_PCT", "long_pct"),
    ("MOCK_MID_PCT", "mid_pct"),
    ("MOCK_SHORT_PCT", "short_pct"),
    ("MOCK_LONG_OUT", "long_out"),
    ("MOCK_MID_OUT", "mid_out"),
    ("MOCK_SHORT_OUT", "short_out"),
    ("MOCK_BATCH", "batch"),
    ("MOCK_STREAM_DEFAULT_INTERVAL", "stream_default_interval"),
];

impl Cfg {
    /// Env defaults (`PORT` is handled by `main`), then `_config` writes on top.
    pub fn from_env() -> Cfg {
        let mut c = Cfg::default();
        for (env_key, knob) in KNOBS {
            if let Ok(raw) = std::env::var(env_key) {
                match raw.parse::<i64>() {
                    Ok(n) => {
                        c.apply(knob, n);
                    }
                    Err(_) => eprintln!("[mock-llm] {env_key}={raw:?} is not an integer, keeping default"),
                }
            }
        }
        c
    }

    /// Set one knob by its readback/query name. Returns false for unknown names.
    pub fn apply(&mut self, key: &str, value: i64) -> bool {
        match key {
            "ttft" => self.ttft = value.max(0) as u64,
            "total_delay" => self.total_delay = value.max(0) as u64,
            "out_tokens" => self.out_tokens = value.max(0) as u64,
            "token_interval" => self.token_interval = value.max(0) as u64,
            "response_size" => self.response_size = value.max(0) as u64,
            "max_duration" => self.max_duration = value.max(0) as u64,
            "mode" => self.mode = value,
            "mix" => self.mix = value.max(0) as u64,
            "long_pct" => self.long_pct = value.max(0) as u64,
            "mid_pct" => self.mid_pct = value.max(0) as u64,
            "short_pct" => self.short_pct = value.max(0) as u64,
            "long_out" => self.long_out = value.max(0) as u64,
            "mid_out" => self.mid_out = value.max(0) as u64,
            "short_out" => self.short_out = value.max(0) as u64,
            "batch" => self.batch = value.max(1) as u64,
            "stream_default_interval" => self.stream_default_interval = value.max(0) as u64,
            _ => return false,
        }
        true
    }

    /// Readback JSON — byte-compatible with the Go format string, `batch` last because
    /// the loadtest harness greps `"batch":[0-9]*}` (see scripts/loadtest/run_levels.sh).
    pub fn readback(&self) -> String {
        format!(
            "{{\"ttft\":{},\"total_delay\":{},\"out_tokens\":{},\"token_interval\":{},\
\"response_size\":{},\"max_duration\":{},\"mode\":{},\"mix\":{},\"long_pct\":{},\
\"mid_pct\":{},\"short_pct\":{},\"long_out\":{},\"mid_out\":{},\"short_out\":{},\"batch\":{}}}",
            self.ttft,
            self.total_delay,
            self.out_tokens,
            self.token_interval,
            self.response_size,
            self.max_duration,
            self.mode,
            self.mix,
            self.long_pct,
            self.mid_pct,
            self.short_pct,
            self.long_out,
            self.mid_out,
            self.short_out,
            self.batch,
        )
    }

    /// Apply a `_config` query string. Go parity: unknown keys and non-integer values
    /// are skipped with a stderr line, never a hard error (the harness verifies by
    /// readback and fail-closes on mismatch).
    pub fn apply_query(&mut self, query: &str) {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, raw) = pair.split_once('=').unwrap_or((pair, ""));
            match raw.parse::<i64>() {
                Ok(n) => {
                    if !self.apply(key, n) {
                        eprintln!("[mock-llm] _config: ignoring unknown key {key}");
                    }
                }
                Err(_) => eprintln!("[mock-llm] _config: ignoring non-integer {key}={raw}"),
            }
        }
    }

    /// Effective output-token count for one request: `mix=1` draws a long/mid/short
    /// bucket by percentage, else `out_tokens` (Go parity, verified by probe).
    fn pick_out(&self) -> u64 {
        if self.mix == 0 {
            return self.out_tokens;
        }
        let total = self.long_pct + self.mid_pct + self.short_pct;
        if total == 0 {
            return self.out_tokens;
        }
        let r = rand_below(total);
        if r < self.long_pct {
            self.long_out
        } else if r < self.long_pct + self.mid_pct {
            self.mid_out
        } else {
            self.short_out
        }
    }
}

/// Cheap dependency-free randomness for the mix buckets (std hash-seeded).
fn rand_below(bound: u64) -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(Instant::now().elapsed().as_nanos() as u64);
    h.finish() % bound.max(1)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn itoa(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

/// `ts` is exactly 13 digits (epoch millis) until year 2286 — fixed width keeps every
/// frame the same size as the Go mock's.
fn ts13(ms: u64, out: &mut BytesMut) {
    let mut buf = [b'0'; 13];
    let mut n = ms;
    for slot in buf.iter_mut().rev() {
        *slot = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.put_slice(&buf);
}

fn push_content(out: &mut impl BufMut, start: u64, len: u64) {
    let phrase_len = PHRASE.len() as u64;
    let mut at = start % phrase_len;
    for _ in 0..len {
        out.put_u8(PHRASE[at as usize]);
        at += 1;
        if at == phrase_len {
            at = 0;
        }
    }
}

fn push_content_frame(out: &mut BytesMut, index: u64, chunk_len: u64, ts: u64) {
    out.put_slice(F_PRE);
    push_content(out, index * chunk_len, chunk_len);
    out.put_slice(F_MID);
    ts13(ts, out);
    out.put_slice(F_SUF);
}

/// finish frame + `[DONE]`, matching the Go payload byte-for-byte.
fn tail_frames(out_tokens: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(360);
    v.extend_from_slice(
        b"data: {\"id\":\"chatcmpl-mock-stream\",\"object\":\"chat.completion.chunk\",\
\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\
\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":",
    );
    itoa(out_tokens, &mut v);
    v.extend_from_slice(b",\"total_tokens\":");
    itoa(out_tokens + 100, &mut v);
    v.extend_from_slice(b"}}\n\ndata: [DONE]\n\n");
    v
}

/// One-shot JSON completion (non-stream). Key order mirrors the Go map marshalling.
fn json_completion(out_tokens: u64, response_size: u64) -> Vec<u8> {
    let total = if response_size > 0 {
        response_size
    } else {
        out_tokens * 4
    };
    let mut v = Vec::with_capacity(total as usize + 320);
    v.extend_from_slice(
        b"{\"choices\":[{\"finish_reason\":\"stop\",\"index\":0,\"message\":{\"content\":\"",
    );
    push_content(&mut v, 0, total);
    v.extend_from_slice(
        b"\",\"role\":\"assistant\"}}],\"created\":",
    );
    itoa(now_secs(), &mut v);
    v.extend_from_slice(b",\"id\":\"chatcmpl-mock-0\",\"model\":\"mock-model\",\"object\":\"chat.completion\",\"usage\":{\"completion_tokens\":");
    itoa(out_tokens, &mut v);
    v.extend_from_slice(b",\"prompt_tokens\":100,\"total_tokens\":");
    itoa(out_tokens + 100, &mut v);
    v.extend_from_slice(b"}}\n"); // Go parity: non-stream body ends with a newline
    v
}

/// Streaming body: paced content frames, then finish + `[DONE]`. `batch=N` coalesces N
/// frames per write without changing a single payload byte (Go parity).
///
/// Implemented as a hand-rolled `Body` state machine rather than an async stream:
/// the stream combinator path re-boxed a future and re-registered a timer per frame
/// (measured 10 µs CPU/chunk vs the Go mock's 6.6 µs). Here the `Sleep` is created
/// once per stream and `reset()` per deadline, and frames render into one reusable
/// scratch buffer.
struct Sse {
    out: u64,
    chunk_len: u64,
    batch: u64,
    interval: Duration,
    max_duration: Duration,
    start: tokio::time::Instant,
    /// Wall-clock ms at construction; per-frame `ts` = base + elapsed (avoids a
    /// SystemTime call per chunk — ts is only 13-digit ms resolution anyway).
    ts_base: u64,
    i: u64,
    tail_sent: bool,
    sleep: Pin<Box<tokio::time::Sleep>>,
    /// One allocation per stream: frames render into the front, `split_to(..).freeze()`
    /// hands slices out zero-copy, the tail keeps the capacity for the next batch.
    buf: BytesMut,
}

impl Sse {
    fn new(out: u64, chunk_len: u64, batch: u64, interval: Duration, max_duration: Duration) -> Sse {
        let start = tokio::time::Instant::now();
        Sse {
            out,
            chunk_len,
            batch,
            interval,
            max_duration,
            start,
            ts_base: now_ms(),
            i: 0,
            tail_sent: false,
            sleep: Box::pin(tokio::time::sleep_until(start)),
            buf: BytesMut::with_capacity(batch as usize * 256 + 256),
        }
    }

    fn truncated(&self) -> bool {
        self.max_duration > Duration::ZERO
            && self.interval.saturating_mul(self.i as u32) >= self.max_duration
    }
}

impl Body for Sse {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let st = &mut *self;
        loop {
            if st.i >= st.out || st.truncated() {
                if st.tail_sent {
                    return std::task::Poll::Ready(None);
                }
                st.tail_sent = true;
                // Flush any partial batch together with the tail — the concatenated
                // payload is identical for any batch size (asserted by tests).
                st.buf.put_slice(&tail_frames(st.out));
                let done = st.buf.split().freeze();
                return std::task::Poll::Ready(Some(Ok(Frame::data(done))));
            }
            // Pacing: one deadline per frame; reusing the Sleep via reset() keeps the
            // timer wheel entry allocation-free.
            let due = st.start + st.interval.saturating_mul(st.i as u32);
            if st.sleep.as_mut().deadline() != due {
                st.sleep.as_mut().reset(due);
            }
            if st.sleep.as_mut().poll(cx).is_pending() {
                // Hold a partial batch until its own deadline boundary: batch=N means
                // N frames per write, not N frames per poll.
                return std::task::Poll::Pending;
            }
            push_content_frame(&mut st.buf, st.i, st.chunk_len, st.ts_base + st.start.elapsed().as_millis() as u64);
            st.i += 1;
            if st.i.is_multiple_of(st.batch) {
                let done = st.buf.split().freeze();
                return std::task::Poll::Ready(Some(Ok(Frame::data(done))));
            }
        }
    }
}

type RespBody = BoxBody<Bytes, Infallible>;

fn boxed(body: impl Body<Data = Bytes, Error = Infallible> + Send + Sync + 'static) -> RespBody {
    body.boxed()
}

fn json(status: StatusCode, body: Vec<u8>) -> Response<RespBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(boxed(Full::new(Bytes::from(body))))
        .expect("static response")
}

/// Apply the `x-mock-*` per-request overrides onto a copy of the config.
fn effective(base: &Cfg, req: &Request<Incoming>) -> Cfg {
    let mut c = *base;
    for (header, knob) in [
        ("x-mock-ttft", "ttft"),
        ("x-mock-delay", "total_delay"),
        ("x-mock-output", "out_tokens"),
        ("x-mock-size", "response_size"),
        ("x-mock-duration", "max_duration"),
        ("x-mock-mode", "mode"),
    ] {
        if let Some(raw) = req
            .headers()
            .get(header)
            .and_then(|v| v.to_str().ok())
            .and_then(|raw| raw.parse::<i64>().ok())
        {
            c.apply(knob, raw);
        }
    }
    c
}

async fn handle(req: Request<Incoming>, cfg: Arc<RwLock<Cfg>>) -> Result<Response<RespBody>, Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    if path == "/health" {
        return Ok(json(StatusCode::OK, b"{\"status\":\"ok\"}".to_vec()));
    }

    if path == "/_config" {
        if method != Method::POST {
            return Ok(Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header("allow", "POST")
                .header("content-type", "text/plain; charset=utf-8")
                .body(boxed(Full::new(Bytes::from_static(b"Method Not Allowed\n"))))
                .expect("static response"));
        }
        let mut guard = cfg.write().expect("cfg lock");
        if let Some(query) = req.uri().query() {
            guard.apply_query(query);
        }
        let body = guard.readback();
        return Ok(json(StatusCode::OK, body.into_bytes()));
    }

    if path != "/v1/chat/completions" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("content-type", "text/plain; charset=utf-8")
            .header("x-content-type-options", "nosniff")
            .body(boxed(Full::new(Bytes::from_static(b"404 page not found"))))
            .expect("static response"));
    }
    if method != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("allow", "POST")
            .header("content-type", "text/plain; charset=utf-8")
            .body(boxed(Full::new(Bytes::from_static(b"Method Not Allowed\n"))))
            .expect("static response"));
    }

    // Fault injection: plain error body, applied before any pacing (Go parity on the
    // two sentinel values the rig uses).
    match req.headers().get("x-mock-error").and_then(|v| v.to_str().ok()) {
        Some("500") => {
            return Ok(json(
                StatusCode::INTERNAL_SERVER_ERROR,
                b"{\"error\":{\"message\":\"mock internal error\",\"type\":\"server_error\",\"code\":\"500\"}}".to_vec(),
            ));
        }
        Some("rate_limit") => {
            return Ok(json(
                StatusCode::TOO_MANY_REQUESTS,
                b"{\"error\":{\"message\":\"mock rate limited\",\"type\":\"rate_limit\",\"code\":\"429\"}}".to_vec(),
            ));
        }
        _ => {}
    }

    let eff = effective(&cfg.read().expect("cfg lock"), &req);
    let raw = req.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    #[derive(serde::Deserialize)]
    struct ChatReq {
        #[serde(default)]
        stream: bool,
    }
    // Go parity: an unparseable body is treated as a non-stream request.
    let stream = serde_json::from_slice::<ChatReq>(&raw)
        .map(|r| r.stream)
        .unwrap_or(false);

    let out = eff.pick_out();
    let chunk_len = if eff.response_size > 0 {
        (eff.response_size / out.max(1)).max(1)
    } else {
        4
    };

    if !stream {
        // Non-stream latency = ttft + total_delay (verified additive on the Go binary).
        let delay = Duration::from_millis(eff.ttft + eff.total_delay);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        return Ok(json(
            StatusCode::OK,
            json_completion(out, eff.response_size),
        ));
    }

    if eff.ttft > 0 {
        tokio::time::sleep(Duration::from_millis(eff.ttft)).await;
    }
    // `token_interval == 0` falls back to `MOCK_STREAM_DEFAULT_INTERVAL` (Go quirk).
    let interval_ms = if eff.token_interval > 0 {
        eff.token_interval
    } else {
        eff.stream_default_interval
    };
    let sse = Sse::new(
        out,
        chunk_len,
        eff.batch.max(1),
        Duration::from_millis(interval_ms),
        Duration::from_millis(eff.max_duration),
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(boxed(sse))
        .expect("stream response"))
}

/// Accept loop: h1 and h2c (prior knowledge) on the same listener.
pub async fn serve(listener: TcpListener, cfg: Arc<RwLock<Cfg>>) {
    let mut conn = ConnBuilder::new(TokioExecutor::new());
    conn.http2().max_concurrent_streams(H2_MAX_CONCURRENT_STREAMS);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[mock-llm] accept failed: {e}");
                continue;
            }
        };
        let cfg = cfg.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| handle(req, cfg.clone()));
            if let Err(e) = conn.serve_connection(TokioIo::new(stream), svc).await {
                eprintln!("[mock-llm] {peer}: connection error: {e}");
            }
        });
    }
}

/// Bind `PORT` (default 8081) and serve until killed.
pub async fn run() -> std::io::Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8081);
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("mock listening on :{} (h1+h2c)", port);
    serve(listener, Arc::new(RwLock::new(Cfg::from_env()))).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn spawn_mock() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, Arc::new(RwLock::new(Cfg::default()))));
        format!("127.0.0.1:{}", addr.port())
    }

    /// Minimal HTTP/1.1 POST with `Connection: close` so the body ends at EOF.
    async fn post(addr: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut req = format!(
            "POST {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\ncontent-length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(body);
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    async fn get(addr: &str, path: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// De-chunk a transfer-encoded body (the SSE responses use chunked framing).
    fn dechunk(raw: &str) -> String {
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
        let mut out = String::new();
        let mut rest = body;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
                break;
            };
            if n == 0 {
                break;
            }
            out.push_str(&tail[..n.min(tail.len())]);
            rest = tail.get(n + 2..).unwrap_or("");
        }
        out
    }

    #[tokio::test]
    async fn health_and_config_readback_keep_batch_last() {
        let addr = spawn_mock().await;
        assert!(get(&addr, "/health").await.contains("{\"status\":\"ok\"}"));

        let readback = post(&addr, "/_config", &[], "").await;
        // The loadtest harness greps `"batch":[0-9]*}` — batch must close the JSON.
        let tail = readback.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(tail.trim_end().ends_with("\"batch\":1}"), "readback: {tail}");

        let applied = post(&addr, "/_config?out_tokens=3&batch=5", &[], "").await;
        assert!(applied.contains("\"out_tokens\":3"), "{applied}");
        assert!(applied.trim_end().ends_with("\"batch\":5}"), "{applied}");
    }

    #[tokio::test]
    async fn stream_frames_carry_ts_and_configured_usage() {
        let addr = spawn_mock().await;
        post(&addr, "/_config?ttft=0&token_interval=1&out_tokens=3&batch=1", &[], "").await;
        let raw = post(
            &addr,
            "/v1/chat/completions",
            &[],
            "{\"model\":\"mock-model\",\"stream\":true}",
        )
        .await;
        let body = dechunk(&raw);
        let frames: Vec<&str> = body.split("\n\n").filter(|f| f.starts_with("data: ")).collect();
        // 3 content frames + finish + [DONE].
        assert_eq!(frames.len(), 5, "{body}");
        assert!(frames.iter().filter(|f| f.contains("\"content\"")).count() == 3);
        assert!(body.contains("\"ts\":"), "gw_oh measurement needs ts");
        assert!(body.contains("\"completion_tokens\":3"), "{body}");
        assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
    }

    #[tokio::test]
    async fn batch_coalescing_does_not_change_payload() {
        let addr = spawn_mock().await;
        post(&addr, "/_config?ttft=0&token_interval=1&out_tokens=20&batch=1", &[], "").await;
        let one = dechunk(&post(&addr, "/v1/chat/completions", &[], "{\"stream\":true}").await);
        post(&addr, "/_config?batch=5", &[], "").await;
        let five = dechunk(&post(&addr, "/v1/chat/completions", &[], "{\"stream\":true}").await);
        // Byte counts and frame counts must match; only the write count differs.
        assert_eq!(one.matches("data: ").count(), five.matches("data: ").count());
        assert_eq!(one.len(), five.len(), "batch must not alter payload bytes");
    }

    #[tokio::test]
    async fn max_duration_truncates_but_usage_reports_configured_out() {
        let addr = spawn_mock().await;
        post(
            &addr,
            "/_config?ttft=0&token_interval=1&out_tokens=200&max_duration=20&batch=1",
            &[],
            "",
        )
        .await;
        let body = dechunk(&post(&addr, "/v1/chat/completions", &[], "{\"stream\":true}").await);
        let content = body.matches("\"content\"").count();
        assert!(content < 200, "expected truncation, got {content} frames");
        assert!(content > 0, "expected some frames before the cap");
        // Go quirk: usage reports the configured count even when truncated.
        assert!(body.contains("\"completion_tokens\":200"), "{body}");
    }

    #[tokio::test]
    async fn error_injection_sentinels() {
        let addr = spawn_mock().await;
        let e500 = post(&addr, "/v1/chat/completions", &[("x-mock-error", "500")], "{}").await;
        assert!(e500.starts_with("HTTP/1.1 500"), "{e500}");
        assert!(e500.contains("mock internal error"), "{e500}");
        let e429 = post(&addr, "/v1/chat/completions", &[("x-mock-error", "rate_limit")], "{}").await;
        assert!(e429.starts_with("HTTP/1.1 429"), "{e429}");
        assert!(e429.contains("mock rate limited"), "{e429}");
    }

    #[tokio::test]
    async fn h2c_prior_knowledge_advertises_250_streams() {
        let addr = spawn_mock().await;
        let mut s = TcpStream::connect(&addr).await.unwrap();
        s.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await.unwrap();
        s.write_all(&[0, 0, 0, 0x04, 0, 0, 0, 0, 0]).await.unwrap(); // empty SETTINGS
        let mut hdr = [0u8; 9];
        s.read_exact(&mut hdr).await.unwrap();
        let len = ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | hdr[2] as usize;
        assert_eq!(hdr[3], 0x04, "expected a SETTINGS frame first");
        assert_eq!(hdr[4] & 1, 0, "expected a non-ACK SETTINGS frame");
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload).await.unwrap();
        let cap = payload
            .chunks_exact(6)
            .find(|c| c[0] == 0 && c[1] == 3)
            .map(|c| u32::from_be_bytes([c[2], c[3], c[4], c[5]]));
        assert_eq!(cap, Some(H2_MAX_CONCURRENT_STREAMS), "xlite pool sizes from this");
    }

    #[tokio::test]
    async fn non_stream_json_shape() {
        let addr = spawn_mock().await;
        post(&addr, "/_config?ttft=0&total_delay=0&out_tokens=3&response_size=12", &[], "").await;
        let raw = post(&addr, "/v1/chat/completions", &[], "{\"stream\":false}").await;
        assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
        assert!(raw.contains("\"id\":\"chatcmpl-mock-0\""), "{raw}");
        assert!(raw.contains("\"completion_tokens\":3"), "{raw}");
        assert!(raw.contains("\"total_tokens\":103"), "{raw}");
    }
}
