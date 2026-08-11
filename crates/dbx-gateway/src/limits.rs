use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{GatewayError, GatewayErrorCode};

struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

pub struct ConnectionRateLimiter {
    rate_per_second: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl ConnectionRateLimiter {
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        Self { rate_per_second: f64::from(rate_per_second), burst: f64::from(burst), buckets: Mutex::default() }
    }

    pub fn allow(&self, identity: &str, now: Instant) -> bool {
        let Ok(mut buckets) = self.buckets.lock() else { return false };
        let bucket = buckets.entry(identity.to_string()).or_insert(Bucket { tokens: self.burst, updated_at: now });
        let elapsed = now.saturating_duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_second).min(self.burst);
        bucket.updated_at = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

pub struct IdentityConcurrency {
    limit: usize,
    identities: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl IdentityConcurrency {
    pub fn new(limit: usize) -> Self {
        Self { limit, identities: Mutex::default() }
    }

    pub fn try_acquire(&self, identity: &str) -> Option<OwnedSemaphorePermit> {
        let semaphore = self
            .identities
            .lock()
            .ok()?
            .entry(identity.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.limit)))
            .clone();
        semaphore.try_acquire_owned().ok()
    }
}

pub struct BufferBudget {
    bytes: Arc<Semaphore>,
}

impl BufferBudget {
    pub fn new(bytes: usize) -> Self {
        Self { bytes: Arc::new(Semaphore::new(bytes)) }
    }

    pub fn try_reserve(&self, bytes: usize) -> Option<OwnedSemaphorePermit> {
        let bytes = u32::try_from(bytes).ok()?;
        self.bytes.clone().try_acquire_many_owned(bytes).ok()
    }
}

#[derive(Clone, Copy)]
pub struct TargetPolicy {
    allow_remote: bool,
}

impl TargetPolicy {
    pub fn new(allow_remote: bool) -> Self {
        Self { allow_remote }
    }

    pub async fn resolve_and_validate(&self, address: &str) -> Result<Vec<SocketAddr>, GatewayError> {
        let addresses = tokio::net::lookup_host(address)
            .await
            .map_err(|_| limit_error(GatewayErrorCode::TargetUnavailable, "target address could not be resolved"))?
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|address| !self.allowed_ip(address.ip())) {
            return Err(limit_error(GatewayErrorCode::RouteDenied, "target address policy rejected"));
        }
        let mut unique = Vec::new();
        for address in addresses {
            if !unique.contains(&address) {
                unique.push(address);
            }
        }
        Ok(unique)
    }

    fn allowed_ip(&self, address: IpAddr) -> bool {
        if address.is_loopback() {
            return true;
        }
        if !self.allow_remote || address.is_unspecified() || address.is_multicast() {
            return false;
        }
        match address {
            IpAddr::V4(address) => {
                !address.is_link_local()
                    && !address.is_broadcast()
                    && address != Ipv4Addr::new(255, 255, 255, 255)
                    && address != Ipv4Addr::new(169, 254, 169, 254)
            }
            IpAddr::V6(address) => !address.is_unicast_link_local(),
        }
    }
}

#[derive(Default, Serialize)]
pub struct SecurityEvent<'a> {
    pub request_id: Option<&'a str>,
    pub peer_role: Option<&'a str>,
    pub peer_id: Option<&'a str>,
    pub cert_serial: Option<&'a str>,
    pub edge_id: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub stage: Option<&'a str>,
    pub bytes_in: Option<u64>,
    pub bytes_out: Option<u64>,
    pub error_code: Option<GatewayErrorCode>,
}

impl SecurityEvent<'_> {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"error_code\":\"internal\"}".to_string())
    }
}

fn limit_error(code: GatewayErrorCode, message: impl Into<String>) -> GatewayError {
    GatewayError { code, message: message.into() }
}
