//! WebSocket transport: the same byte stream as the TCP protocol, carried in
//! binary WebSocket messages of at most `chunk` bytes. This is what passes
//! through HTTPS-only proxies such as Cloudflare Tunnel; large attachments are
//! streamed as many small messages, so no single HTTP request body is involved.

use crate::relay::Io;
use futures_util::{SinkExt, StreamExt};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// A stream with bytes already read off the front put back.
pub struct Rewind<S> {
    pre: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Rewind<S> {
    pub fn new(pre: Vec<u8>, inner: S) -> Self {
        Self { pre, pos: 0, inner }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.pos < self.pre.len() {
            let n = (self.pre.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.pre[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, b: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// What the HTTP upgrade request said about the original client (set by a proxy).
#[derive(Default, Clone)]
pub struct Forwarded {
    /// The proxy received the request over HTTPS.
    pub https: bool,
    pub client_ip: Option<String>,
}

/// Completes the WebSocket handshake on `stream` and returns a plain byte stream
/// bridged to it.
pub async fn accept<S: Io + 'static>(stream: S, chunk: usize) -> Result<(DuplexStream, Forwarded), String> {
    let seen = Arc::new(Mutex::new(Forwarded::default()));
    let s2 = seen.clone();
    let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp: Response| {
        let h = req.headers();
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let https = get("x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https"))
            || get("cf-visitor").is_some_and(|v| v.contains("\"https\""));
        let ip = get("cf-connecting-ip")
            .or_else(|| get("x-forwarded-for").and_then(|f| f.split(',').next().map(|s| s.trim().to_string())))
            .filter(|s| !s.is_empty() && s.len() <= 64);
        *s2.lock().unwrap() = Forwarded { https, client_ip: ip };
        Ok(resp)
    })
    .await
    .map_err(|e| e.to_string())?;
    let fwd = seen.lock().unwrap().clone();
    Ok((bridge(ws, chunk), fwd))
}

/// Pumps bytes between a WebSocket and the returned duplex stream.
pub fn bridge<S: Io + 'static>(ws: tokio_tungstenite::WebSocketStream<S>, chunk: usize) -> DuplexStream {
    let (ours, theirs) = tokio::io::duplex(chunk.max(16 * 1024) * 2);
    let (mut sink, mut source) = ws.split();
    let (mut rd, mut wr) = tokio::io::split(theirs);
    tokio::spawn(async move {
        let mut buf = vec![0u8; chunk];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::binary(buf[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });
    tokio::spawn(async move {
        while let Some(Ok(m)) = source.next().await {
            match m {
                Message::Binary(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {} // pings are answered by tungstenite
            }
        }
        let _ = wr.shutdown().await;
    });
    ours
}
