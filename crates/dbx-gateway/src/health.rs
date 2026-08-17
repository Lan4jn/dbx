use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::main_gateway::EdgeRegistry;
use crate::tls::load_certificates;
use crate::{GatewayError, GatewayErrorCode};

pub struct HealthServer {
    address: SocketAddr,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    pub async fn bind(
        listen: &str,
        registry: EdgeRegistry,
        certificate_path: &Path,
        pki_configured: bool,
    ) -> Result<Self, GatewayError> {
        let requested: SocketAddr = listen.parse().map_err(|_| health_error("health listen address is invalid"))?;
        if !matches!(requested.ip(), IpAddr::V4(ip) if ip.is_loopback())
            && !matches!(requested.ip(), IpAddr::V6(ip) if ip.is_loopback())
        {
            return Err(health_error("health listener must bind a loopback management address"));
        }
        let listener =
            TcpListener::bind(requested).await.map_err(|_| health_error("health listener could not bind"))?;
        let address = listener.local_addr().map_err(|_| health_error("health listener address unavailable"))?;
        let certificate = load_certificates(certificate_path)?;
        let (_, certificate) = x509_parser::parse_x509_certificate(certificate[0].as_ref())
            .map_err(|_| health_error("health certificate status unavailable"))?;
        let certificate_not_after = certificate.validity().not_after.to_datetime().unix_timestamp();
        let process_id = std::process::id();
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        let Ok((mut stream, _)) = result else { break };
                        let registry = registry.clone();
                        tokio::spawn(async move {
                            let mut request = [0_u8; 1024];
                            let Ok(count) = stream.read(&mut request).await else { return };
                            let healthy_path = request[..count].starts_with(b"GET /healthz ");
                            let online_edges = registry.read().await.values().filter(|entry| entry.online).count();
                            let body = if healthy_path {
                                format!(
                                    "{{\"status\":\"ok\",\"process_id\":{process_id},\"server_certificate_not_after_unix\":{certificate_not_after},\"pki_configured\":{pki_configured},\"online_edges\":{online_edges},\"database_checks\":0}}"
                                )
                            } else {
                                String::new()
                            };
                            let status = if healthy_path { "200 OK" } else { "404 Not Found" };
                            let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        });
                    }
                    changed = stopping.changed() => {
                        if changed.is_err() || *stopping.borrow() { break; }
                    }
                }
            }
        });
        Ok(Self { address, stop, task })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

fn health_error(message: impl Into<String>) -> GatewayError {
    GatewayError { code: GatewayErrorCode::ConfigInvalid, message: message.into() }
}
