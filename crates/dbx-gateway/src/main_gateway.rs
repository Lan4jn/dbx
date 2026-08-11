use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::Empty;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;

use crate::config::MainConfig;
use crate::tls::{GatewayTls, PeerIdentity};
use crate::{GatewayError, GatewayErrorCode};

pub struct MainGateway {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl MainGateway {
    pub async fn bind(config: MainConfig) -> Result<Self, GatewayError> {
        let tls = Arc::new(GatewayTls::load(&config)?);
        let listener =
            TcpListener::bind(&config.listen).await.map_err(|_| internal_error("main listener could not bind"))?;
        let local_addr = listener.local_addr().map_err(|_| internal_error("main listener address unavailable"))?;
        let acceptor = TlsAcceptor::from(tls.server_config.clone());
        let edge_path = Arc::<str>::from(config.edge_path);
        let dbx_path = Arc::<str>::from(config.dbx_path);
        let connection_slots = Arc::new(Semaphore::new(config.max_connections));
        let tls_handshake_timeout = Duration::from_secs(config.tls_handshake_timeout_secs);
        let http_header_timeout = Duration::from_secs(config.http_header_timeout_secs);
        let (shutdown, mut stop) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let accepted = tokio::select! {
                    _ = &mut stop => break,
                    completed = connections.join_next(), if !connections.is_empty() => {
                        let _ = completed;
                        continue;
                    }
                    result = listener.accept() => result,
                };
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = connection_slots.clone().try_acquire_owned() else {
                    continue;
                };
                let acceptor = acceptor.clone();
                let tls = tls.clone();
                let edge_path = edge_path.clone();
                let dbx_path = dbx_path.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let Ok(Ok(stream)) = timeout(tls_handshake_timeout, acceptor.accept(stream)).await else {
                        return;
                    };
                    let Ok(identity) = tls.classify(stream.get_ref().1.peer_certificates()) else {
                        return;
                    };
                    let (first_request_tx, first_request_rx) = oneshot::channel();
                    let first_request_tx = Arc::new(Mutex::new(Some(first_request_tx)));
                    let service = service_fn(move |mut request: Request<Incoming>| {
                        if let Ok(mut sender) = first_request_tx.lock() {
                            if let Some(sender) = sender.take() {
                                let _ = sender.send(());
                            }
                        }
                        request.extensions_mut().insert(identity.clone());
                        route(request, edge_path.clone(), dbx_path.clone())
                    });
                    let mut builder = auto::Builder::new(TokioExecutor::new());
                    builder.http1().timer(TokioTimer::new()).header_read_timeout(http_header_timeout);
                    let connection = builder.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = &mut connection => {}
                        first = timeout(http_header_timeout, first_request_rx) => {
                            if first.is_ok() {
                                connection.as_mut().graceful_shutdown();
                                let _ = connection.await;
                            }
                        }
                    }
                });
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(Self { local_addr, shutdown: Some(shutdown), task })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = (&mut self.task).await;
    }
}

impl Drop for MainGateway {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn route(
    request: Request<Incoming>,
    edge_path: Arc<str>,
    dbx_path: Arc<str>,
) -> Result<Response<Empty<Bytes>>, Infallible> {
    let identity = request.extensions().get::<PeerIdentity>();
    let allowed = matches!(identity, Some(PeerIdentity::Edge { .. })) && request.uri().path() == edge_path.as_ref()
        || matches!(identity, Some(PeerIdentity::DbxClient { .. })) && request.uri().path() == dbx_path.as_ref();
    let status = if allowed { StatusCode::OK } else { StatusCode::NOT_FOUND };
    Ok(Response::builder().status(status).body(Empty::new()).expect("fixed response is valid"))
}

fn internal_error(message: &str) -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: message.to_string() }
}
