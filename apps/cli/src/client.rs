//! The JSON-RPC client the one-shot commands use: over the socket of a running `serve --socket`,
//! or in-process (a [`Server`] session on an in-memory pipe), so both run the same code.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dbservice::Service;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value as Json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::rpc::{self, Incoming, RpcError, INTERNAL_ERROR};
use crate::server::{serve_stream, Server, Transport};

pub struct Notification {
    pub method: String,
    pub params: Json,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Json, RpcError>>>>>;

pub struct Client {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    notifications: Mutex<Option<mpsc::UnboundedReceiver<Notification>>>,
    next: AtomicU64,
}

fn gone() -> RpcError {
    RpcError::new(INTERNAL_ERROR, "The server closed the connection.")
}

impl Client {
    pub fn over<R, W>(read: R, write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut write = write;
            while let Some(line) = rx.recv().await {
                if write.write_all(format!("{line}\n").as_bytes()).await.is_err() || write.flush().await.is_err() {
                    break;
                }
            }
        });
        let pending: Pending = Default::default();
        let (notify, notifications) = mpsc::unbounded_channel();
        let reading = pending.clone();
        tokio::spawn(async move {
            let mut read = BufReader::new(read);
            let mut line = Vec::new();
            loop {
                line.clear();
                match read.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(message) = serde_json::from_slice::<Incoming>(&line) else { continue };
                if let Some(method) = message.method {
                    let _ = notify.send(Notification { method, params: message.params.unwrap_or(Json::Null) });
                    continue;
                }
                let Some(id) = message.id.as_ref().and_then(Json::as_u64) else { continue };
                let outcome = match (message.result, message.error) {
                    (_, Some(error)) => Err(error),
                    (result, None) => Ok(result.unwrap_or(Json::Null)),
                };
                if let Some(waiter) = reading.lock().unwrap_or_else(|p| p.into_inner()).remove(&id) {
                    let _ = waiter.send(outcome);
                }
            }
            for (_, waiter) in reading.lock().unwrap_or_else(|p| p.into_inner()).drain() {
                let _ = waiter.send(Err(gone()));
            }
        });
        Self { out, pending, notifications: Mutex::new(Some(notifications)), next: AtomicU64::new(1) }
    }

    /// A server session of this process's own.
    pub fn in_process(service: Service) -> Self {
        let (near, far) = tokio::io::duplex(1 << 16);
        let (far_read, far_write) = tokio::io::split(far);
        tokio::spawn(serve_stream(Server::new(service, Transport::InProcess), Transport::InProcess, far_read, far_write));
        let (read, write) = tokio::io::split(near);
        Self::over(read, write)
    }

    #[cfg(unix)]
    pub async fn connect(path: &Path) -> std::io::Result<Self> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (read, write) = stream.into_split();
        Ok(Self::over(read, write))
    }

    #[cfg(not(unix))]
    pub async fn connect(_path: &Path) -> std::io::Result<Self> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "sockets are Unix only"))
    }

    /// Sends a request; the id lets `query.cancel` name it.
    pub fn request<P: Serialize>(&self, method: &str, params: P) -> (u64, impl Future<Output = Result<Json, RpcError>> + use<P>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id, tx);
        let params = serde_json::to_value(params).unwrap_or_else(|_| json!({}));
        let sent = self.out.send(rpc::request(&json!(id), method, &params)).is_ok();
        (id, async move {
            if !sent {
                return Err(gone());
            }
            rx.await.unwrap_or_else(|_| Err(gone()))
        })
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: impl Serialize) -> Result<T, RpcError> {
        let json = self.request(method, params).1.await?;
        decode(json)
    }

    /// Notifications (dump and restore progress) as they arrive, kept until read. Once.
    pub fn notifications(&self) -> mpsc::UnboundedReceiver<Notification> {
        self.notifications.lock().unwrap_or_else(|p| p.into_inner()).take().expect("notifications taken twice")
    }
}

pub fn decode<T: DeserializeOwned>(json: Json) -> Result<T, RpcError> {
    serde_json::from_value(json).map_err(|e| RpcError::new(INTERNAL_ERROR, format!("Unexpected answer from the server: {e}")))
}
