//! HTTP reverse proxy over the relay pipe (`docs/relay/pipe.md`).
//!
//! A remote client (the native app) opens a relayed byte-stream pipe with
//! `stream_type = "http-local-proxy"` and treats it as a raw TCP connection to
//! Skald's local web server. This loop accepts those pipes and splices each one,
//! byte-for-byte, to a fresh `127.0.0.1:<web_port>` connection — a transparent
//! tunnel (we never parse HTTP, so HTTP/1.1 keep-alive, parallel connections, and
//! the chat WebSocket upgrade all work). The destination is pinned to the local
//! web port: the client cannot choose host/port, so this never becomes an open
//! proxy to other local services.
//!
//! Access is already gated by the relay: only the namespace agent or an authorized
//! client can establish a pipe (`pipe.md §3.1`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

use skald_relay_client::{IncomingPipe, PipeConnection, RelayClient};

use crate::PLUGIN_ID;

/// Pipe `stream_type` this loop handles. The native app sets the same value when
/// opening the pipe.
pub(crate) const HTTP_LOCAL_PROXY_STREAM_TYPE: &str = "http-local-proxy";

/// Read buffer for the local→remote direction. Bounded so a pipe can't buffer
/// unboundedly (the relay also caps the data-plane frame size).
const READ_BUF: usize = 64 * 1024;

/// Monotonic per-connection id for log correlation across concurrent tunnels.
static CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Subscribe to inbound pipe invites and reverse-proxy `http-local-proxy` pipes
/// to the local web server. One spawned task per accepted pipe.
pub(crate) async fn run_proxy_loop(
    client: Arc<RelayClient>,
    web_port: u16,
    cancel: CancellationToken,
) {
    let mut rx = client.incoming_pipes();
    debug!(plugin = PLUGIN_ID, web_port, "http-local-proxy: listening for pipe invites");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            ev = rx.recv() => match ev {
                Ok(incoming) => {
                    trace!(
                        plugin = PLUGIN_ID,
                        stream_type = %incoming.stream_type,
                        from = %hex::encode(incoming.from),
                        "http-local-proxy: pipe invite received",
                    );
                    // Ignore (don't reject) other stream_types: `incoming_pipes` is a
                    // broadcast, so a future consumer may legitimately want them.
                    if incoming.stream_type != HTTP_LOCAL_PROXY_STREAM_TYPE {
                        trace!(plugin = PLUGIN_ID, stream_type = %incoming.stream_type,
                               "http-local-proxy: stream_type not ours, ignoring");
                        continue;
                    }
                    let client = Arc::clone(&client);
                    let child  = cancel.child_token();
                    tokio::spawn(async move {
                        accept_and_proxy(client, incoming, web_port, child).await;
                    });
                }
                Err(RecvError::Lagged(n)) => {
                    warn!(plugin = PLUGIN_ID, skipped = n, "incoming pipes lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
    debug!(plugin = PLUGIN_ID, "http-local-proxy: invite loop stopped");
}

/// Accept one invite, then proxy it. Runs in its own task.
async fn accept_and_proxy(
    client:   Arc<RelayClient>,
    incoming: IncomingPipe,
    web_port: u16,
    cancel:   CancellationToken,
) {
    let conn = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    debug!(plugin = PLUGIN_ID, conn, from = %hex::encode(incoming.from),
           "http-local-proxy: accepting pipe");
    let pipe = match client.accept_pipe(&incoming).await {
        Ok(p) => p,
        Err(e) => {
            warn!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: accept_pipe failed");
            return;
        }
    };
    debug!(plugin = PLUGIN_ID, conn, "http-local-proxy: pipe accepted, opening local connection");
    proxy_one(conn, pipe, web_port, cancel).await;
}

/// Splice a pipe to a fresh local TCP connection until either side closes.
///
/// Full-duplex: the pipe is `split` into independent send/receive halves, each
/// driven by its own task, so the two directions never block each other (a
/// stalled write on one side can't hold up the other). `PipeSender::send`
/// applies backpressure internally (it blocks only when the pipe's send buffer
/// is full), and `recv`/`read` are cancel-safe. When either direction ends it
/// cancels the shared token so the other unwinds; dropping both pipe halves
/// closes the socket.
async fn proxy_one(conn: u64, pipe: PipeConnection, port: u16, cancel: CancellationToken) {
    let tcp = match TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(e) => {
            warn!(plugin = PLUGIN_ID, conn, port, error = %e, "http-local-proxy: local connect failed");
            pipe.close().await;
            return;
        }
    };
    debug!(plugin = PLUGIN_ID, conn, port, "http-local-proxy: local connection established");
    let (mut rd, mut wr) = tcp.into_split();
    let (mut tx, mut rx) = pipe.split();
    let to_local = Arc::new(AtomicU64::new(0)); // remote → local bytes
    let to_remote = Arc::new(AtomicU64::new(0)); // local → remote bytes

    // remote → local: decrypted client bytes forwarded to the web server.
    let mut rl = {
        let cancel = cancel.clone();
        let to_local = Arc::clone(&to_local);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return "cancelled",
                    r = rx.recv() => match r {
                        Ok(Some(bytes)) => {
                            trace!(plugin = PLUGIN_ID, conn, n = bytes.len(), "http-local-proxy: remote→local");
                            if let Err(e) = wr.write_all(&bytes).await {
                                debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: local write failed");
                                return "local write error";
                            }
                            to_local.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        }
                        Ok(None) => return "remote closed",
                        Err(e) => {
                            debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: pipe recv error");
                            return "pipe recv error";
                        }
                    },
                }
            }
        })
    };

    // local → remote: web-server bytes sealed back over the pipe.
    let mut lr = {
        let cancel = cancel.clone();
        let to_remote = Arc::clone(&to_remote);
        tokio::spawn(async move {
            let mut buf = vec![0u8; READ_BUF];
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return "cancelled",
                    r = rd.read(&mut buf) => match r {
                        Ok(0) => return "local EOF",
                        Ok(n) => {
                            trace!(plugin = PLUGIN_ID, conn, n, "http-local-proxy: local→remote");
                            if let Err(e) = tx.send(&buf[..n]).await {
                                debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: pipe send failed");
                                return "pipe send error";
                            }
                            to_remote.fetch_add(n as u64, Ordering::Relaxed);
                        }
                        Err(e) => {
                            debug!(plugin = PLUGIN_ID, conn, error = %e, "http-local-proxy: local read error");
                            return "local read error";
                        }
                    },
                }
            }
        })
    };

    // First direction to finish decides the reason; cancel the survivor and join
    // **only** it. The handle resolved inside `select!` is already complete and
    // must not be polled again (`JoinHandle polled after completion`). Joining
    // the survivor lets both pipe halves drop so the socket closes cleanly.
    let reason = tokio::select! {
        r = &mut rl => {
            cancel.cancel();
            let _ = lr.await;
            r.unwrap_or("remote→local task panic")
        }
        r = &mut lr => {
            cancel.cancel();
            let _ = rl.await;
            r.unwrap_or("local→remote task panic")
        }
    };
    debug!(plugin = PLUGIN_ID, conn,
           to_local = to_local.load(Ordering::Relaxed),
           to_remote = to_remote.load(Ordering::Relaxed),
           reason, "http-local-proxy: pipe closed");
}
