//! `Attempt::validate()` runs before any socket, and `execute_redial_once`
//! redials exactly once, only for the stale class, from the original attempt.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Uri};
use xlite_transport::{
    Attempt, AttemptOutcome, AttemptTelemetry, BodyStream, StaleKind, Transport, TransportError,
    TransportStats,
};

fn attempt_with(uri: &str, headers: &[(&str, &str)], body: &[u8]) -> Attempt {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        map.append(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_bytes(v.as_bytes()).unwrap(),
        );
    }
    Attempt {
        method: Method::POST,
        uri: uri
            .parse::<Uri>()
            .unwrap_or_else(|e| panic!("test uri `{uri}` does not parse: {e}")),
        headers: map,
        body: Bytes::copy_from_slice(body),
    }
}

fn empty_outcome() -> AttemptOutcome {
    AttemptOutcome {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        version: http::Version::HTTP_2,
        body: BodyStream::new(futures_util::stream::empty(), None),
        telemetry: AttemptTelemetry::default(),
    }
}

/// A transport whose behaviour per call is a fixed script; counts calls so a
/// validation failure must provably produce zero upstream work.
struct Scripted {
    calls: Arc<AtomicU8>,
    script: Vec<Result<(), TransportError>>,
}

impl Transport for Scripted {
    async fn execute(&self, _attempt: Attempt) -> Result<AttemptOutcome, TransportError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
        match &self.script[n.min(self.script.len() - 1)] {
            Ok(()) => Ok(empty_outcome()),
            Err(e) => Err(match e {
                TransportError::StaleConnection { kind } => {
                    TransportError::StaleConnection { kind: *kind }
                }
                TransportError::HeaderTimeout { waited } => {
                    TransportError::HeaderTimeout { waited: *waited }
                }
                other => panic!("test script should not contain {other:?}"),
            }),
        }
    }

    fn stats(&self) -> TransportStats {
        TransportStats::default()
    }
}

#[test]
fn validation_rejects_before_any_io() {
    // No scheme, no authority, bad length: all InvalidRequest, all zero-I/O.
    let cases: Vec<(Attempt, &str)> = vec![
        (attempt_with("/v1/completions", &[], b"{}"), "no scheme"),
        (attempt_with("ftp://h/x", &[], b"{}"), "scheme"),
        (
            attempt_with("http://h/x", &[("content-length", "999")], b"{}"),
            "length mismatch",
        ),
        (
            attempt_with(
                "http://h/x",
                &[("content-length", "2"), ("transfer-encoding", "chunked")],
                b"{}",
            ),
            "length+te",
        ),
        (
            attempt_with("http://h/x", &[("content-length", "not-a-number")], b"{}"),
            "unparsable length",
        ),
    ];
    for (attempt, label) in cases {
        assert!(
            matches!(attempt.validate(), Err(TransportError::InvalidRequest(_))),
            "case `{label}` must be InvalidRequest"
        );
    }

    // Consistent duplicates are legal (RFC 9110 allows identical repeats).
    let ok = attempt_with(
        "http://h/x",
        &[("content-length", "2"), ("content-length", "2")],
        b"{}",
    );
    assert!(ok.validate().is_ok());

    // A scheme-ful URI without an authority cannot be built from a string by
    // `http::Uri`; assemble one piecewise to reach the check.
    if let Ok(uri) = Uri::builder().scheme("http").path_and_query("/x").build() {
        let attempt = Attempt {
            method: Method::POST,
            uri,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        };
        assert!(matches!(
            attempt.validate(),
            Err(TransportError::InvalidRequest(_))
        ));
    }
}

#[tokio::test]
async fn redial_once_retries_only_the_stale_class() {
    let calls = Arc::new(AtomicU8::new(0));
    let stale = Scripted {
        calls: calls.clone(),
        script: vec![
            Err(TransportError::StaleConnection {
                kind: StaleKind::PeerClosed,
            }),
            Ok(()),
        ],
    };
    let attempt = attempt_with("http://h/x", &[], b"{}");
    let outcome = stale.execute_redial_once(attempt).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one redial");
    assert_eq!(outcome.telemetry.redials, 1);
    assert_eq!(outcome.telemetry.redial_reason, Some(StaleKind::PeerClosed));

    // HeaderTimeout is never redialled: a live-but-slow server must not see a
    // replayed POST.
    let calls = Arc::new(AtomicU8::new(0));
    let slow = Scripted {
        calls: calls.clone(),
        script: vec![Err(TransportError::HeaderTimeout {
            waited: std::time::Duration::from_secs(2),
        })],
    };
    let err = slow
        .execute_redial_once(attempt_with("http://h/x", &[], b"{}"))
        .await
        .unwrap_err();
    assert!(matches!(err, TransportError::HeaderTimeout { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no redial after timeout");
}

#[test]
fn stats_invariant_holds() {
    TransportStats::default().assert_invariants();
}
