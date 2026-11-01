//! Passing one HTTP request through to an upstream, streaming both ways.
//!
//! Shared by the node gateway and the E2B front door. Both used to be, or
//! would have been, a reqwest call that read the whole request body, sent
//! it, then read the whole answer before replying -- which cannot carry
//! envd: `commands.run` is a server stream that stays open for as long as
//! the command runs, and the SDK reads each chunk as it comes. So this
//! speaks HTTP/1.1 on a connection of its own, hands the caller's body to
//! the upstream as it arrives, and returns the upstream's body unread.
//!
//! A connection per request rather than a pool: the gateway's upstream is a
//! sandbox on the same machine, and one connection can then never be reused
//! against a different sandbox. Connecting is also how the gateway learns a
//! sandbox is cold (#327 makes it refuse), which has to happen before any of
//! the request body is consumed -- a streamed body cannot be sent twice.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, Request, Response, StatusCode};
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

/// Headers that describe one hop, not the message (RFC 9110 7.6.1).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Open a connection to the upstream. Kept apart from `send` so a caller
/// can tell "nothing is listening" from anything that goes wrong later.
pub async fn connect(addr: SocketAddr, timeout: Duration) -> std::io::Result<TcpStream> {
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(r) => {
            let s = r?;
            // Small writes are the norm here: a PTY echo, a stream frame.
            let _ = s.set_nodelay(true);
            Ok(s)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("connecting to {addr} took longer than {timeout:?}"),
        )),
    }
}

/// Whether a connect error means the upstream is refusing -- a cold
/// sandbox, or one whose program has not bound its port yet.
pub fn is_refused(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ConnectionRefused
}

/// Whether a connect error says the sandbox is not running, so a gateway
/// should wake it: refused, as a frozen sandbox is behind its REJECT
/// (#327), or no route at all, as one on disk is -- its namespace is gone
/// and its address falls on the node's unreachable floor (`net.rs`).
///
/// Only for a node's own gateway, where the address is a sandbox's. Before
/// #365 every connect error woke; from #365 until this, only a refusal did,
/// and a sandbox on disk answered every request with a 502 and was never
/// woken: 67-79% of requests in the fleet's traffic runs.
pub fn is_cold(e: &std::io::Error) -> bool {
    is_refused(e)
        || matches!(
            e.raw_os_error(),
            Some(libc::EHOSTUNREACH) | Some(libc::ENETUNREACH)
        )
}

/// Send `req` on `stream` and return the answer with its body still
/// streaming. An upgrade (a WebSocket) is spliced through: once both sides
/// have switched protocols, bytes are copied until either closes.
pub async fn send(
    stream: TcpStream,
    mut req: Request<Body>,
) -> Result<Response<Body>, hyper::Error> {
    let upgrading = wants_upgrade(req.headers());
    let downstream = upgrading.then(|| hyper::upgrade::on(&mut req));
    strip_hop_by_hop(req.headers_mut(), upgrading);

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        // Ends when the exchange does; an error here is the same one the
        // response or its body reports, so it is not logged twice.
        let _ = conn.with_upgrades().await;
    });
    let mut res = sender.send_request(req).await?;

    let switched = res.status() == StatusCode::SWITCHING_PROTOCOLS;
    if let (true, Some(downstream)) = (switched, downstream) {
        let upstream = hyper::upgrade::on(&mut res);
        tokio::spawn(async move {
            let (Ok(down), Ok(up)) = (downstream.await, upstream.await) else {
                return;
            };
            let _ =
                tokio::io::copy_bidirectional(&mut TokioIo::new(down), &mut TokioIo::new(up)).await;
        });
    }
    strip_hop_by_hop(res.headers_mut(), switched);
    Ok(res.map(Body::new))
}

fn wants_upgrade(h: &HeaderMap) -> bool {
    h.contains_key(header::UPGRADE)
        && h.get_all(header::CONNECTION).iter().any(|v| {
            v.to_str().is_ok_and(|s| {
                s.split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
            })
        })
}

/// Drop what belongs to the hop just crossed, keeping `Connection` and
/// `Upgrade` when the point of the request is to switch protocols.
fn strip_hop_by_hop(h: &mut HeaderMap, upgrading: bool) {
    // Headers the sender named in `Connection` are hop-by-hop too.
    let named: Vec<HeaderName> = h
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for n in named {
        if upgrading && (n == header::UPGRADE || n == header::CONNECTION) {
            continue;
        }
        h.remove(n);
    }
    for n in HOP_BY_HOP {
        if upgrading && (*n == "connection" || *n == "upgrade") {
            continue;
        }
        h.remove(*n);
    }
}

/// A body that holds something until it has been read to the end or
/// dropped: the gateway's in-flight guard, so a sandbox streaming a
/// ten-minute command's output is in flight for those ten minutes and not
/// only until its headers arrived.
pub struct Holding<G> {
    inner: Body,
    _held: G,
}

impl<G: Send + Unpin + 'static> Holding<G> {
    pub fn wrap(body: Body, held: G) -> Body {
        Body::new(Self {
            inner: body,
            _held: held,
        })
    }
}

impl<G: Unpin> http_body::Body for Holding<G> {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_dropped_and_named_ones_too() {
        let mut h = HeaderMap::new();
        h.insert("connection", "keep-alive, x-secret".parse().unwrap());
        h.insert("x-secret", "1".parse().unwrap());
        h.insert("transfer-encoding", "chunked".parse().unwrap());
        h.insert("x-kept", "1".parse().unwrap());
        strip_hop_by_hop(&mut h, false);
        assert!(h.get("x-secret").is_none());
        assert!(h.get("connection").is_none());
        assert!(h.get("transfer-encoding").is_none());
        assert!(h.get("x-kept").is_some());
    }

    #[test]
    fn an_upgrade_keeps_what_it_needs() {
        let mut h = HeaderMap::new();
        h.insert("connection", "Upgrade".parse().unwrap());
        h.insert("upgrade", "websocket".parse().unwrap());
        assert!(wants_upgrade(&h));
        strip_hop_by_hop(&mut h, true);
        assert!(h.get("connection").is_some());
        assert!(h.get("upgrade").is_some());
    }

    /// The point of the module: the first chunk arrives while the upstream
    /// is still writing the rest.
    #[tokio::test]
    async fn a_response_streams_rather_than_arriving_whole() {
        use http_body_util::BodyExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            s.write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nfirst\r\n")
                .await
                .unwrap();
            // The rest only once the client has the first chunk.
            rx.await.unwrap();
            s.write_all(b"6\r\nsecond\r\n0\r\n\r\n").await.unwrap();
        });
        let stream = connect(addr, Duration::from_secs(2)).await.unwrap();
        let req = Request::builder()
            .uri("/")
            .header("host", "x")
            .body(Body::empty())
            .unwrap();
        let res = send(stream, req).await.unwrap();
        let mut body = res.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(&first[..], b"first");
        tx.send(()).unwrap();
        let second = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(&second[..], b"second");
    }

    #[tokio::test]
    async fn nothing_listening_is_a_refusal() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let e = connect(addr, Duration::from_secs(2)).await.unwrap_err();
        assert!(is_refused(&e), "{e:?}");
    }

    #[test]
    fn a_sandbox_on_disk_is_cold() {
        use std::io::{Error, ErrorKind};
        // On disk: no route, from the node's unreachable floor.
        assert!(is_cold(&Error::from_raw_os_error(libc::EHOSTUNREACH)));
        assert!(is_cold(&Error::from_raw_os_error(libc::ENETUNREACH)));
        // Frozen: refused by its REJECT.
        assert!(is_cold(&Error::from_raw_os_error(libc::ECONNREFUSED)));
        // A slow guest is not cold, and waking it would not help.
        assert!(!is_cold(&Error::from(ErrorKind::TimedOut)));
        assert!(!is_cold(&Error::from_raw_os_error(libc::ECONNRESET)));
        // The front door still tells a refusal from an unreachable node.
        assert!(!is_refused(&Error::from_raw_os_error(libc::EHOSTUNREACH)));
    }
}
