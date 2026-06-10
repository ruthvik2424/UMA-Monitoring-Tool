//! Persistent WSS connection to the collector.
//!
//! Design priorities:
//!   - Persistent connection — no TLS handshake per alert.
//!   - Critical alerts have a dedicated mpsc lane that bypasses the normal queue.
//!   - TCP_NODELAY on every socket; immediate flush after each send.
//!   - Pre-allocated send buffer so a critical send never allocates.
//!   - Exponential backoff on reconnect with jitter.

use anyhow::{anyhow, Context, Result};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use std::convert::TryFrom;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Receiver;
use tokio::time::sleep;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    tungstenite::{
        client::IntoClientRequest,
        protocol::{Message, WebSocketConfig},
    },
    WebSocketStream,
};
use tracing::{debug, info, warn};

use crate::bus::Outbound;
use crate::config::AgentConfig;

type WsStream = WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

/// Run the transport loop forever. Drains both the normal and critical
/// channels and ships their contents on the WSS. Reconnects with exponential
/// backoff if the connection drops.
pub async fn run(
    cfg: AgentConfig,
    mut normal_rx: Receiver<Outbound>,
    mut critical_rx: Receiver<Outbound>,
) -> ! {
    let mut backoff_ms = cfg.collector.reconnect_min_ms;
    let mut send_buf = BytesMut::with_capacity(64 * 1024);

    loop {
        match connect(&cfg).await {
            Ok(mut ws) => {
                info!(url = %cfg.collector.url, "connected to collector");
                backoff_ms = cfg.collector.reconnect_min_ms;

                if let Err(e) = pump(&mut ws, &mut normal_rx, &mut critical_rx, &mut send_buf).await {
                    warn!(error = %e, "transport pump exited");
                }
                let _ = ws.close(None).await;
            }
            Err(e) => {
                warn!(error = ?e, "collector connect failed; backing off");
            }
        }
        sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = backoff_ms.saturating_mul(2).min(cfg.collector.reconnect_max_ms);
    }
}

async fn pump(
    ws: &mut WsStream,
    normal: &mut Receiver<Outbound>,
    critical: &mut Receiver<Outbound>,
    buf: &mut BytesMut,
) -> Result<()> {
    let mut ping_tick = tokio::time::interval(Duration::from_secs(15));
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased; // critical lane gets first look every poll

            Some(out) = critical.recv() => {
                ship(ws, out, buf, true).await?;
            }
            Some(out) = normal.recv() => {
                ship(ws, out, buf, false).await?;
            }
            _ = ping_tick.tick() => {
                ws.send(Message::Ping(Vec::new().into())).await
                    .context("sending keepalive ping")?;
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Close(c))) => return Err(anyhow!("server closed: {:?}", c)),
                    Some(Err(e)) => return Err(anyhow!("ws read error: {e}")),
                    None => return Err(anyhow!("ws stream ended")),
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

async fn ship(ws: &mut WsStream, out: Outbound, buf: &mut BytesMut, critical: bool) -> Result<()> {
    let env = out.into_envelope();
    buf.clear();
    serde_json::to_writer((&mut *buf).writer(), &env).context("serialize envelope")?;
    let payload = std::str::from_utf8(buf).context("non-utf8 envelope")?;
    ws.send(Message::Text(payload.to_owned().into()))
        .await
        .context("send ws frame")?;
    if critical {
        ws.flush().await.context("flush critical alert")?;
    }
    Ok(())
}

async fn connect(cfg: &AgentConfig) -> Result<WsStream> {
    let request = cfg.collector.url.as_str().into_client_request()
        .context("parsing collector URL")?;
    let uri = request.uri().clone();
    let host = uri.host().ok_or_else(|| anyhow!("collector url missing host"))?.to_string();
    let port = uri.port_u16().unwrap_or(9443);

    let stream = TcpStream::connect((host.as_str(), port))
        .await
        .with_context(|| format!("tcp connect {}:{}", host, port))?;
    stream.set_nodelay(true).ok();
    set_keepalive(&stream, Duration::from_secs(cfg.collector.keepalive_s)).ok();

    let tls_cfg = build_tls_client(cfg).context("building tls config")?;
    let connector = TlsConnector::from(Arc::new(tls_cfg));
    let sni = ServerName::try_from(host.clone())
        .map_err(|e| anyhow!("invalid SNI host '{host}': {e}"))?;
    let tls = connector.connect(sni, stream).await.context("tls handshake")?;

    let ws_config = WebSocketConfig {
        max_message_size: Some(2 * 1024 * 1024),
        max_frame_size: Some(2 * 1024 * 1024),
        ..Default::default()
    };
    let (ws, _) = tokio_tungstenite::client_async_with_config(request, tls, Some(ws_config))
        .await
        .map_err(|e| anyhow!("ws handshake: {e}"))?;
    debug!("ws handshake complete");
    Ok(ws)
}

fn build_tls_client(cfg: &AgentConfig) -> Result<rustls::ClientConfig> {
    use rustls::RootCertStore;

    let mut roots = RootCertStore::empty();
    let ca_pem = std::fs::read(&cfg.tls.ca_cert)
        .with_context(|| format!("reading CA cert {}", cfg.tls.ca_cert.display()))?;
    let mut ca_reader = std::io::BufReader::new(ca_pem.as_slice());
    for cert in rustls_pemfile::certs(&mut ca_reader) {
        let cert = cert.context("parsing CA cert")?;
        roots.add(cert).context("adding CA cert to root store")?;
    }

    let client_certs: Vec<_> = {
        let pem = std::fs::read(&cfg.tls.client_cert)
            .with_context(|| format!("reading client cert {}", cfg.tls.client_cert.display()))?;
        let mut r = std::io::BufReader::new(pem.as_slice());
        rustls_pemfile::certs(&mut r)
            .collect::<std::result::Result<_, _>>()
            .context("parsing client cert chain")?
    };

    let key = {
        let pem = std::fs::read(&cfg.tls.client_key)
            .with_context(|| format!("reading client key {}", cfg.tls.client_key.display()))?;
        let mut r = std::io::BufReader::new(pem.as_slice());
        rustls_pemfile::private_key(&mut r)
            .context("parsing client key")?
            .ok_or_else(|| anyhow!("no private key found in {}", cfg.tls.client_key.display()))?
    };

    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(client_certs, key)
        .context("installing client auth cert")?)
}

fn set_keepalive(stream: &TcpStream, interval: Duration) -> std::io::Result<()> {
    use nix::libc::{
        self, setsockopt, IPPROTO_TCP, SOL_SOCKET, SO_KEEPALIVE, TCP_KEEPCNT, TCP_KEEPIDLE,
        TCP_KEEPINTVL,
    };
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let secs: libc::c_int = interval.as_secs().min(i32::MAX as u64) as libc::c_int;
    let yes: libc::c_int = 1;
    let cnt: libc::c_int = 3;
    unsafe {
        let _ = setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE,
            &yes as *const _ as *const _, std::mem::size_of_val(&yes) as _);
        let _ = setsockopt(fd, IPPROTO_TCP, TCP_KEEPIDLE,
            &secs as *const _ as *const _, std::mem::size_of_val(&secs) as _);
        let _ = setsockopt(fd, IPPROTO_TCP, TCP_KEEPINTVL,
            &secs as *const _ as *const _, std::mem::size_of_val(&secs) as _);
        let _ = setsockopt(fd, IPPROTO_TCP, TCP_KEEPCNT,
            &cnt as *const _ as *const _, std::mem::size_of_val(&cnt) as _);
    }
    Ok(())
}

// Tiny helper trait so we can write into BytesMut via std::io::Write.
trait BytesMutExt {
    fn writer(self) -> bytes::buf::Writer<Self> where Self: Sized;
}
impl BytesMutExt for &mut BytesMut {
    fn writer(self) -> bytes::buf::Writer<Self> {
        bytes::BufMut::writer(self)
    }
}
