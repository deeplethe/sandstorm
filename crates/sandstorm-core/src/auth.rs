//! Bearer-token authentication middleware.
//!
//! When `--token-file` is set on the daemon, every request except
//! `/healthz` must carry `Authorization: Bearer <tok>` matching the
//! token's contents (whitespace-trimmed). The file is read once at
//! startup; rotating the token requires a daemon restart.
//!
//! When the token file is absent, the daemon runs unauthenticated.
//! Loopback-only binds (the default `127.0.0.1:8080`) make this safe
//! for a single-tenant developer setup. For any multi-tenant or
//! non-loopback deployment, supply `--token-file`.
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

/// Same `{"error": "..."}` shape the master's own handlers return, so a
/// client sees one error format whichever layer refused it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Clone)]
pub struct AuthConfig {
    /// Expected bearer token. `None` means the daemon is unauthenticated.
    pub token: Option<Arc<String>>,
}

impl AuthConfig {
    pub fn open() -> Self {
        Self { token: None }
    }

    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: Some(Arc::new(token.into())),
        }
    }
}

/// axum middleware that gates every route on a valid bearer token,
/// except `/healthz` which always returns 200 so load balancers can
/// probe the daemon without a credential.
///
/// The bypass accepts both `/healthz` and `/healthz/` — the middleware
/// runs before route dispatch in axum 0.7, so without normalising the
/// trailing slash a probe at `/healthz/` would hit the auth check
/// instead of the route, return 401, and make the daemon look down to
/// a load balancer that munged the path (k8s sidecars, curl
/// `--location`-style normalisation). URI paths are case-sensitive per
/// RFC 3986, so we don't fold case.
pub async fn require_token(cfg: AuthConfig, req: Request, next: Next) -> Response {
    if is_healthz_path(req.uri().path()) {
        return next.run(req).await;
    }
    let Some(expected) = cfg.token.as_ref() else {
        return next.run(req).await;
    };

    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let presented = header.strip_prefix("Bearer ").unwrap_or("").trim();

    if presented.is_empty() {
        return reject(StatusCode::UNAUTHORIZED, "missing bearer token");
    }
    if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return reject(StatusCode::UNAUTHORIZED, "invalid bearer token");
    }
    next.run(req).await
}

/// True for `/healthz` and `/healthz/` (no other variants — uppercase
/// is rejected because URIs are case-sensitive, embedded encodings like
/// `/healthz%2F` are rejected because we don't decode before
/// comparing). See [#196](https://github.com/deeplethe/sandstorm/issues/196).
fn is_healthz_path(path: &str) -> bool {
    path == "/healthz" || path == "/healthz/"
}

fn reject(status: StatusCode, msg: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: msg.to_string(),
        }),
    )
        .into_response()
}

/// Constant-time byte comparison of a presented bearer token against
/// the daemon's expected token.
///
/// Backed by `subtle::ConstantTimeEq`, which compiles to a length-
/// equal `ct_eq` and is the standard answer for this problem in Rust
/// crypto code. We pad the presented token (in a fresh allocation) so
/// the comparison path is the same regardless of input length — this
/// closes the length-oracle vector reported in issue #162.
///
/// History: the previous hand-rolled implementation had two flaws —
///
/// 1. On length mismatch it executed a "fake work" loop using
///    `wrapping_mul(0)`, which the compiler is allowed to (and does)
///    erase as dead code, leaving response time monotonic in the
///    longer slice's length.
/// 2. Even if the loop had been preserved, taking different branches
///    for length-mismatch vs length-equal is itself a timing oracle:
///    the length-equal branch contains a real XOR loop, the mismatch
///    branch a constant-bytes loop.
///
/// The fix here uses one code path regardless of input length and
/// defers timing-resistance to `subtle`.
/// Compare a presented secret against an expected one in constant time.
///
/// Exposed because the API token is no longer the only secret on the wire:
/// each sandbox has its own access token, and comparing that with `==`
/// would reintroduce the length oracle of issue #162 one layer down.
pub fn secrets_match(presented: &str, expected: &str) -> bool {
    constant_time_eq(presented.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(presented: &[u8], expected: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    // Build a length-`expected.len()` view of the presented token so
    // the comparison always runs on equal-length buffers. Bytes past
    // `presented.len()` are zero-filled; any difference (extra bytes,
    // missing bytes, wrong bytes) shows up as a non-zero XOR in the
    // ct_eq result.
    let n = expected.len();
    let mut padded = vec![0u8; n];
    let take = presented.len().min(n);
    padded[..take].copy_from_slice(&presented[..take]);
    // Combine the ct_eq result with a length-equality bit so a
    // presented token that is a strict prefix of `expected` (padded
    // would equal `expected` on the first n bytes but presented is
    // shorter) is still rejected. Both branches reduce to the same
    // arithmetic.
    let bytes_match: bool = padded.ct_eq(expected).into();
    // Length comparison is already constant-time at the CPU level.
    // Non-short-circuiting bitwise AND keeps both sides evaluated so
    // the function's total time doesn't depend on bytes_match's value.
    bytes_match & (presented.len() == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_tokens_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn different_tokens_not_eq() {
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
    }

    #[test]
    fn presented_prefix_of_expected_rejected() {
        // Regression for #162: an attacker who guesses a prefix of the
        // real token would have been distinguishable from a totally
        // wrong guess in the old implementation. Now both reject.
        assert!(!constant_time_eq(b"abc", b"abcdef"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn presented_longer_than_expected_rejected() {
        // Symmetric case: padded view truncates to expected's length,
        // so an attacker can't bypass by appending garbage.
        assert!(!constant_time_eq(b"abcdef", b"abc"));
        assert!(!constant_time_eq(b"x", b""));
    }

    #[test]
    fn healthz_bypass_accepts_both_slash_variants() {
        // Regression for #196: probes at `/healthz/` (trailing slash)
        // hit the auth check instead of the route and returned 401,
        // breaking k8s liveness probes that normalise trailing slashes.
        assert!(is_healthz_path("/healthz"));
        assert!(is_healthz_path("/healthz/"));
    }

    #[test]
    fn healthz_bypass_rejects_neighbours() {
        // Anything that isn't exactly /healthz or /healthz/ must still
        // require auth — including case-folded forms (URI paths are
        // case-sensitive per RFC 3986), percent-encoded slashes (we
        // don't decode before comparing), and nested subpaths.
        assert!(!is_healthz_path("/Healthz"));
        assert!(!is_healthz_path("/HEALTHZ"));
        assert!(!is_healthz_path("/healthz/sub"));
        assert!(!is_healthz_path("/healthz//"));
        assert!(!is_healthz_path("/healthz%2F"));
        assert!(!is_healthz_path("/healthz?token=x"));
        assert!(!is_healthz_path(""));
        assert!(!is_healthz_path("/"));
    }

    #[test]
    fn all_zero_padding_doesnt_accidentally_match() {
        // If `presented` is shorter than `expected`, the unfilled
        // tail of `padded` is 0x00. Make sure that doesn't accidentally
        // match an expected token whose tail also happens to be 0x00.
        assert!(!constant_time_eq(b"ab", b"ab\x00\x00"));
        assert!(!constant_time_eq(b"", b"\x00\x00"));
    }
}
