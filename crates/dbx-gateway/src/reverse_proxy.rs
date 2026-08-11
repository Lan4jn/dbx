use std::convert::Infallible;
use std::net::SocketAddr;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderName, HeaderValue, CONNECTION, HOST, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_PROTOCOL, UPGRADE};
use hyper::{Request, Response, StatusCode, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::{GatewayError, GatewayErrorCode};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ProxyBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

#[derive(Clone)]
pub struct FixedUpstreamProxy {
    upstream: Uri,
    client: Client<HttpsConnector<HttpConnector>, Incoming>,
}

impl FixedUpstreamProxy {
    pub fn new(upstream: &str) -> Result<Self, GatewayError> {
        let upstream: Uri = upstream.parse().map_err(|_| proxy_error("fallback upstream URL is invalid"))?;
        if !matches!(upstream.scheme_str(), Some("http" | "https"))
            || upstream.authority().is_none()
            || upstream.authority().is_some_and(|authority| authority.as_str().contains('@'))
        {
            return Err(proxy_error("fallback upstream URL is invalid"));
        }
        let connector =
            HttpsConnectorBuilder::new().with_webpki_roots().https_or_http().enable_http1().enable_http2().build();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Ok(Self { upstream, client })
    }

    pub async fn fallback(
        &self,
        mut request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Result<Response<ProxyBody>, GatewayError> {
        *request.uri_mut() = self.upstream_uri(request.uri())?;
        let upgrade = request.headers().get(UPGRADE).cloned();
        sanitize_request_headers(&mut request, &self.upstream, peer)?;
        if let Some(upgrade) = &upgrade {
            request.headers_mut().insert(CONNECTION, HeaderValue::from_static("upgrade"));
            request.headers_mut().insert(UPGRADE, upgrade.clone());
        }
        let wants_upgrade = upgrade.is_some();
        let downstream_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut request));
        request.extensions_mut().clear();
        let mut response =
            self.client.request(request).await.map_err(|_| proxy_error("fallback upstream request failed"))?;
        let switched = response.status() == StatusCode::SWITCHING_PROTOCOLS;
        if switched {
            if let Some(downstream_upgrade) = downstream_upgrade {
                let upstream_upgrade = hyper::upgrade::on(&mut response);
                tokio::spawn(async move {
                    let (Ok(downstream), Ok(upstream)) = tokio::join!(downstream_upgrade, upstream_upgrade) else {
                        return;
                    };
                    let mut downstream = TokioIo::new(downstream);
                    let mut upstream = TokioIo::new(upstream);
                    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                });
            }
            let mut downstream = Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header(CONNECTION, "Upgrade")
                .header(UPGRADE, "websocket");
            for name in [SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_PROTOCOL] {
                if let Some(value) = response.headers().get(&name) {
                    downstream = downstream.header(name, value);
                }
            }
            return downstream
                .body(empty_proxy_body())
                .map_err(|_| proxy_error("fallback WebSocket response was invalid"));
        }
        sanitize_response_headers(&mut response);
        Ok(response.map(|body| body.map_err(|error| -> BoxError { Box::new(error) }).boxed()))
    }

    fn upstream_uri(&self, request: &Uri) -> Result<Uri, GatewayError> {
        let base = self.upstream.path().trim_end_matches('/');
        let request_path = request.path().trim_start_matches('/');
        let path = if request_path.is_empty() {
            if base.is_empty() {
                "/".to_string()
            } else {
                base.to_string()
            }
        } else if base.is_empty() {
            format!("/{request_path}")
        } else {
            format!("{base}/{request_path}")
        };
        let path_and_query = match request.query() {
            Some(query) => format!("{path}?{query}"),
            None => path,
        };
        let mut parts = self.upstream.clone().into_parts();
        parts.path_and_query =
            Some(path_and_query.parse().map_err(|_| proxy_error("fallback upstream request path is invalid"))?);
        Uri::from_parts(parts).map_err(|_| proxy_error("fallback upstream request URI is invalid"))
    }
}

pub fn empty_proxy_body() -> ProxyBody {
    Full::new(Bytes::new()).map_err(|never: Infallible| match never {}).boxed()
}

pub fn full_proxy_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes).map_err(|never: Infallible| match never {}).boxed()
}

fn sanitize_request_headers(
    request: &mut Request<Incoming>,
    upstream: &Uri,
    peer: SocketAddr,
) -> Result<(), GatewayError> {
    remove_hop_headers(request.headers_mut());
    let authority = upstream.authority().ok_or_else(|| proxy_error("fallback upstream URL is invalid"))?;
    request
        .headers_mut()
        .insert(HOST, HeaderValue::from_str(authority.as_str()).map_err(|_| proxy_error("upstream Host is invalid"))?);
    append_forwarded_for(request.headers_mut(), peer.ip().to_string())?;
    request.headers_mut().insert("x-forwarded-proto", HeaderValue::from_static("https"));
    Ok(())
}

fn sanitize_response_headers(response: &mut Response<Incoming>) {
    remove_hop_headers(response.headers_mut());
}

fn remove_hop_headers(headers: &mut hyper::HeaderMap) {
    let connection_headers = headers
        .get(CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value.split(',').filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok()).collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for name in connection_headers {
        headers.remove(name);
    }
    for name in [
        CONNECTION,
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-authenticate"),
        HeaderName::from_static("proxy-authorization"),
        HeaderName::from_static("te"),
        HeaderName::from_static("trailer"),
        HeaderName::from_static("transfer-encoding"),
        UPGRADE,
    ] {
        headers.remove(name);
    }
}

fn append_forwarded_for(headers: &mut hyper::HeaderMap, peer: String) -> Result<(), GatewayError> {
    let value = match headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()) {
        Some(existing) if !existing.is_empty() => format!("{existing}, {peer}"),
        _ => peer,
    };
    headers.insert(
        "x-forwarded-for",
        HeaderValue::from_str(&value).map_err(|_| proxy_error("forwarded client address is invalid"))?,
    );
    Ok(())
}

fn proxy_error(message: impl Into<String>) -> GatewayError {
    GatewayError { code: GatewayErrorCode::Internal, message: message.into() }
}
