//! The E2B front door: one listener for `api.{domain}` and every
//! `{port}-{sandboxID}.{domain}`.
//!
//! The E2B SDKs reach a sandbox at `https://{port}-{id}.{domain}`, so
//! something has to read the Host and know where that sandbox is. This
//! does, and nothing more: it looks the sandbox up, passes the request to
//! the gateway on the sandbox's node with the sandbox's token and the port
//! in `X-Sandstorm-Port`, and streams the answer back. Waking a cold
//! sandbox stays the node gateway's job (RFC 15.5: a wake never goes
//! through the master), so the front door holds no tier state and could
//! run as any number of copies.
//!
//! Three ways to name a sandbox, in this order:
//!
//! 1. Host `{port}-{id}.{domain}`. Split at the first `-`: everything before
//!    it is the port, everything after is the id.
//! 2. A path `/sandbox/{id}/{port}/...`, for a caller with no wildcard DNS.
//! 3. Headers `E2b-Sandbox-Id` and `E2b-Sandbox-Port`, which the SDKs send
//!    on every envd request.
//!
//! Anything else, and anything for `api.{domain}`, is the E2B API.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower::ServiceExt;

use super::e2b::{self, E2b, E2bErr};
use super::SANDBOX_TOKEN_HEADER;
use crate::node::PORT_HEADER;
use crate::proxy;

/// Where to listen and what as.
#[derive(Debug, Clone)]
pub struct FrontConfig {
    pub bind: SocketAddr,
    pub domain: String,
    /// A certificate for `*.{domain}` and its key, PEM. When absent one is
    /// issued from the deployment's own CA, which a client then has to be
    /// told to trust (`SSL_CERT_FILE`).
    pub tls_cert: Option<std::path::PathBuf>,
    pub tls_key: Option<std::path::PathBuf>,
}

/// What a request is for.
#[derive(Debug, PartialEq, Eq)]
enum Dest {
    Api,
    Sandbox {
        id: String,
        port: u16,
        /// The path to send, when it is not the request's own: the path
        /// form strips its `/sandbox/{id}/{port}` prefix.
        path: Option<String>,
    },
}

fn host_of(req: &Request) -> Option<String> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))
        .map(|h| {
            // Without the port, case-folded: DNS names are not case-sensitive.
            let h = h.trim().to_ascii_lowercase();
            match h.strip_prefix('[') {
                // An IPv6 literal: the port, if any, follows the bracket.
                Some(v6) => v6.split(']').next().unwrap_or("").to_string(),
                None => h.split(':').next().unwrap_or("").to_string(),
            }
        })
}

/// `{port}-{id}`, split at the FIRST dash. A Sandstorm id made through
/// `/v1` is `sb-...` and has a dash of its own; splitting at the last one
/// would cut it in two.
fn split_label(label: &str) -> Option<(u16, String)> {
    let (port, id) = label.split_once('-')?;
    let port: u16 = port.parse().ok()?;
    if port == 0 || id.is_empty() {
        return None;
    }
    Some((port, id.to_string()))
}

fn classify(
    domain: &str,
    host: Option<&str>,
    headers: &axum::http::HeaderMap,
    path_and_query: &str,
) -> Dest {
    if let Some(h) = host {
        if h == format!("api.{domain}") {
            return Dest::Api;
        }
        if let Some(label) = h.strip_suffix(&format!(".{domain}")) {
            if !label.contains('.') {
                if let Some((port, id)) = split_label(label) {
                    return Dest::Sandbox {
                        id,
                        port,
                        path: None,
                    };
                }
            }
        }
    }
    if let Some(rest) = path_and_query.strip_prefix("/sandbox/") {
        let mut it = rest.splitn(3, '/');
        if let (Some(id), Some(port)) = (it.next(), it.next()) {
            // The port segment may be the end of the path, with a query.
            let (port, query) = port
                .split_once('?')
                .map_or((port, None), |(p, q)| (p, Some(q)));
            if let Ok(port) = port.parse::<u16>() {
                if !id.is_empty() && port > 0 {
                    let tail = it.next().unwrap_or("");
                    let path = match query {
                        Some(q) => format!("/?{q}"),
                        None => format!("/{tail}"),
                    };
                    return Dest::Sandbox {
                        id: id.to_string(),
                        port,
                        path: Some(path),
                    };
                }
            }
        }
    }
    let hv = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim);
    if let (Some(id), Some(port)) = (hv("e2b-sandbox-id"), hv("e2b-sandbox-port")) {
        if let Ok(port) = port.parse::<u16>() {
            if !id.is_empty() && port > 0 {
                return Dest::Sandbox {
                    id: id.to_string(),
                    port,
                    path: None,
                };
            }
        }
    }
    Dest::Api
}

struct Front {
    e2b: Arc<E2b>,
    api: Router,
}

pub fn router(e2b: Arc<E2b>) -> Router {
    let api = e2b::router(e2b.clone());
    Router::new()
        .fallback(dispatch)
        .with_state(Arc::new(Front { e2b, api }))
}

async fn dispatch(State(f): State<Arc<Front>>, req: Request) -> Response {
    let host = host_of(&req);
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    match classify(&f.e2b.domain, host.as_deref(), req.headers(), &pq) {
        Dest::Api => match f.api.clone().oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        },
        Dest::Sandbox { id, port, path } => forward(&f.e2b, &id, port, path, req)
            .await
            .unwrap_or_else(IntoResponse::into_response),
    }
}

/// How long the node's gateway may take to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

async fn forward(
    e: &E2b,
    id: &str,
    port: u16,
    path: Option<String>,
    req: Request,
) -> Result<Response, E2bErr> {
    // 502 with E2B's body for a sandbox that is not there, as E2B's own
    // proxy answers: the SDKs turn it into "sandbox not found".
    let not_found = || {
        E2bErr(
            StatusCode::BAD_GATEWAY,
            format!("The sandbox {id} was not found"),
        )
    };
    let route = e.route(id)?.ok_or_else(not_found)?;

    let stream = match proxy::connect(route.gateway, CONNECT_TIMEOUT).await {
        Ok(s) => s,
        Err(err) => {
            e.forget(id);
            // A detached sandbox is on no node, so nothing listens for it.
            return Err(if proxy::is_refused(&err) {
                E2bErr(
                    StatusCode::BAD_GATEWAY,
                    format!("sandbox {id} is paused or not serving; connect to it to resume it"),
                )
            } else {
                tracing::warn!(sandbox = %id, gateway = %route.gateway, error = %err, "front door could not reach the node");
                E2bErr(
                    StatusCode::BAD_GATEWAY,
                    format!("the node holding sandbox {id} is unreachable: {err}"),
                )
            });
        }
    };

    let (mut parts, body) = req.into_parts();
    // Whatever the caller sent in these is not theirs to send: the token is
    // what this door adds, and the port is what the Host said.
    e2b::strip_inbound(&mut parts.headers);
    let token = route.token.parse().map_err(|_| {
        E2bErr(
            StatusCode::INTERNAL_SERVER_ERROR,
            "sandbox token is not a header value".into(),
        )
    })?;
    parts.headers.insert(SANDBOX_TOKEN_HEADER, token);
    parts.headers.insert(PORT_HEADER, port.into());
    let pq = match path {
        Some(p) => p,
        None => parts
            .uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_else(|| "/".into()),
    };
    parts.uri = pq.parse::<Uri>().unwrap_or_default();

    match proxy::send(stream, Request::from_parts(parts, body)).await {
        Ok(res) => Ok(res),
        Err(err) => {
            e.forget(id);
            Err(E2bErr(
                StatusCode::BAD_GATEWAY,
                format!("forwarding to sandbox {id}: {err}"),
            ))
        }
    }
}

/// Bind the front door. Split from serving so the certificate is issued
/// while the caller holds the CA, and nothing awaits under that lock.
pub fn prepare(
    e2b: Arc<E2b>,
    cfg: FrontConfig,
    ca: &crate::identity::Ca,
) -> Result<impl std::future::Future<Output = Result<()>>> {
    let (cert, key) = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(c), Some(k)) => (crate::tls::read_pem(c)?, crate::tls::read_pem(k)?),
        (None, None) => {
            let paths = ca.issue_front(&e2b.domain)?;
            // The CA goes in the chain so a client trusting only ca.pem can
            // build the path.
            let leaf = crate::tls::read_pem(&paths.cert)?;
            (
                format!("{leaf}{}", ca.cert_pem()),
                crate::tls::read_pem(&paths.key)?,
            )
        }
        _ => anyhow::bail!("--e2b-tls-cert and --e2b-tls-key go together"),
    };
    let mut tls = crate::tls::plain_server_config(&cert, &key)
        .context("loading the E2B front door's certificate")?;
    // HTTP/1.1 only: a WebSocket upgrade is an HTTP/1.1 mechanism, and the
    // SDKs speak nothing else to envd.
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    tracing::info!(
        bind = %cfg.bind,
        api = %format!("https://api.{}", e2b.domain),
        sandboxes = %format!("https://{{port}}-{{id}}.{}", e2b.domain),
        own_ca = cfg.tls_cert.is_none(),
        "E2B front door listening"
    );
    let app = router(e2b);
    Ok(async move {
        axum_server::bind_rustls(
            cfg.bind,
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls)),
        )
        .map(|tls| tls.acceptor(axum_server::accept::NoDelayAcceptor))
        .serve(app.into_make_service())
        .await
        .context("serving the E2B front door")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    const D: &str = "sb.example.com";

    #[test]
    fn the_api_host_is_the_api() {
        assert_eq!(
            classify(
                D,
                Some("api.sb.example.com"),
                &HeaderMap::new(),
                "/sandboxes"
            ),
            Dest::Api
        );
    }

    #[test]
    fn a_sandbox_host_splits_at_the_first_dash() {
        assert_eq!(
            classify(
                D,
                Some("49983-abc234.sb.example.com"),
                &HeaderMap::new(),
                "/files"
            ),
            Dest::Sandbox {
                id: "abc234".into(),
                port: 49983,
                path: None
            }
        );
        // A /v1 id carries a dash of its own.
        assert_eq!(
            classify(
                D,
                Some("8000-sb-0123abcd.sb.example.com"),
                &HeaderMap::new(),
                "/"
            ),
            Dest::Sandbox {
                id: "sb-0123abcd".into(),
                port: 8000,
                path: None
            }
        );
    }

    #[test]
    fn a_host_under_another_domain_is_not_a_sandbox() {
        assert_eq!(
            classify(D, Some("49983-abc.evil.com"), &HeaderMap::new(), "/"),
            Dest::Api
        );
        assert_eq!(
            classify(
                D,
                Some("a.49983-abc.sb.example.com"),
                &HeaderMap::new(),
                "/"
            ),
            Dest::Api
        );
    }

    #[test]
    fn the_path_form_strips_its_prefix() {
        assert_eq!(
            classify(
                D,
                Some("10.0.0.1"),
                &HeaderMap::new(),
                "/sandbox/abc/49983/files?path=/x"
            ),
            Dest::Sandbox {
                id: "abc".into(),
                port: 49983,
                path: Some("/files?path=/x".into())
            }
        );
        assert_eq!(
            classify(D, Some("10.0.0.1"), &HeaderMap::new(), "/sandbox/abc/8000"),
            Dest::Sandbox {
                id: "abc".into(),
                port: 8000,
                path: Some("/".into())
            }
        );
    }

    #[test]
    fn the_headers_name_a_sandbox_when_nothing_else_does() {
        let mut h = HeaderMap::new();
        h.insert("e2b-sandbox-id", "abc".parse().unwrap());
        h.insert("e2b-sandbox-port", "49983".parse().unwrap());
        assert_eq!(
            classify(D, Some("localhost"), &h, "/health"),
            Dest::Sandbox {
                id: "abc".into(),
                port: 49983,
                path: None
            }
        );
        // But not over the API host: the SDK sends them to connect, too.
        assert_eq!(
            classify(D, Some("api.sb.example.com"), &h, "/sandboxes/abc/connect"),
            Dest::Api
        );
    }
}
