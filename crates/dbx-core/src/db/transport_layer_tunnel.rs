use crate::models::connection::TransportLayerConfig;

use super::dbx_gateway::DbxGatewayManager;
use super::http_tunnel::HttpTunnelManager;
use super::proxy_tunnel::ProxyTunnelManager;
use super::ssh_tunnel::TunnelManager;

#[derive(Debug, Clone, PartialEq, Eq)]
struct LayerEndpoint {
    host: String,
    port: u16,
}

impl LayerEndpoint {
    fn localhost(port: u16) -> Self {
        Self { host: "127.0.0.1".to_string(), port }
    }
}

fn layer_endpoint(layer: &TransportLayerConfig) -> LayerEndpoint {
    match layer {
        TransportLayerConfig::DbxGateway(gateway) => url::Url::parse(&gateway.main_url)
            .ok()
            .and_then(|url| {
                Some(LayerEndpoint { host: url.host_str()?.to_string(), port: url.port_or_known_default()? })
            })
            .unwrap_or_else(|| LayerEndpoint { host: gateway.main_url.clone(), port: 0 }),
        _ => {
            let (host, port) = layer.endpoint();
            LayerEndpoint { host: host.to_string(), port }
        }
    }
}

/// Resolves any `~/.ssh/config` aliases on SSH layers before endpoints are
/// computed. Both `.endpoint()` read sites below (the current layer's own
/// connect target, and an earlier layer's forward target when this layer is
/// the *next* one in the chain) must see the resolved host/port rather than
/// a literal alias string, so resolution happens once up front instead of
/// only at the `start_tunnel` call site.
fn resolve_ssh_layers(
    layers: &[TransportLayerConfig],
    resolve: impl Fn(&crate::models::connection::SshTunnelConfig) -> crate::models::connection::SshTunnelConfig,
) -> Vec<TransportLayerConfig> {
    layers
        .iter()
        .map(|layer| match layer {
            TransportLayerConfig::Ssh(ssh) => TransportLayerConfig::Ssh(resolve(ssh)),
            other => other.clone(),
        })
        .collect()
}

/// Starts an ordered transport layer chain and returns the final local port.
///
/// Each layer listens on a local port. The next layer connects to that local
/// port, and the last layer forwards to `remote_host:remote_port`.
pub async fn start_transport_layers(
    connection_id: &str,
    layers: &[TransportLayerConfig],
    remote_host: &str,
    remote_port: u16,
    ssh_tunnels: &TunnelManager,
    proxy_tunnels: &ProxyTunnelManager,
    http_tunnels: &HttpTunnelManager,
    dbx_gateway: &DbxGatewayManager,
) -> Result<u16, String> {
    start_transport_layers_with_final_ssh_local_port(
        connection_id,
        layers,
        remote_host,
        remote_port,
        None,
        ssh_tunnels,
        proxy_tunnels,
        http_tunnels,
        dbx_gateway,
    )
    .await
}

pub(crate) async fn start_transport_layers_with_final_ssh_local_port(
    connection_id: &str,
    layers: &[TransportLayerConfig],
    remote_host: &str,
    remote_port: u16,
    final_ssh_local_port: Option<u16>,
    ssh_tunnels: &TunnelManager,
    proxy_tunnels: &ProxyTunnelManager,
    http_tunnels: &HttpTunnelManager,
    dbx_gateway: &DbxGatewayManager,
) -> Result<u16, String> {
    start_transport_layers_internal(
        connection_id,
        layers,
        remote_host,
        remote_port,
        final_ssh_local_port,
        false,
        ssh_tunnels,
        proxy_tunnels,
        http_tunnels,
        dbx_gateway,
    )
    .await
}

#[cfg(feature = "mq-admin")]
pub(crate) async fn start_transport_layers_with_final_ssh_socks5(
    connection_id: &str,
    layers: &[TransportLayerConfig],
    ssh_tunnels: &TunnelManager,
    proxy_tunnels: &ProxyTunnelManager,
    http_tunnels: &HttpTunnelManager,
    dbx_gateway: &DbxGatewayManager,
) -> Result<u16, String> {
    if !matches!(layers.last(), Some(TransportLayerConfig::Ssh(_))) {
        return Err("Dynamic SOCKS5 routing requires SSH as the final transport layer".to_string());
    }
    start_transport_layers_internal(
        connection_id,
        layers,
        "",
        0,
        None,
        true,
        ssh_tunnels,
        proxy_tunnels,
        http_tunnels,
        dbx_gateway,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn start_transport_layers_internal(
    connection_id: &str,
    layers: &[TransportLayerConfig],
    remote_host: &str,
    remote_port: u16,
    final_ssh_local_port: Option<u16>,
    final_ssh_socks5: bool,
    ssh_tunnels: &TunnelManager,
    proxy_tunnels: &ProxyTunnelManager,
    http_tunnels: &HttpTunnelManager,
    dbx_gateway: &DbxGatewayManager,
) -> Result<u16, String> {
    if layers.is_empty() {
        return Err("No transport layers configured".to_string());
    }
    validate_transport_layers(layers)?;

    let layers = resolve_ssh_layers(layers, crate::ssh_config::resolve_ssh_tunnel_config);
    let layers = layers.as_slice();

    let mut next_connect_endpoint: Option<LayerEndpoint> = None;
    let mut final_local_port = 0;

    for (index, layer) in layers.iter().enumerate() {
        let layer_id = layer_id(connection_id, index);
        let is_last = index + 1 == layers.len();
        let connect_endpoint = next_connect_endpoint.clone().unwrap_or_else(|| layer_endpoint(layer));
        let target_endpoint = if is_last {
            LayerEndpoint { host: remote_host.to_string(), port: remote_port }
        } else {
            layer_endpoint(&layers[index + 1])
        };

        let local_port = match layer {
            TransportLayerConfig::Ssh(resolved) if is_last && final_ssh_socks5 => ssh_tunnels
                .start_socks5_proxy(
                    &layer_id,
                    &connect_endpoint.host,
                    connect_endpoint.port,
                    &resolved.host,
                    resolved.port,
                    &resolved.user,
                    &resolved.password,
                    &resolved.key_path,
                    &resolved.key_passphrase,
                    resolved.use_ssh_agent,
                    &resolved.ssh_agent_sock_path,
                    &resolved.auth_method,
                    effective_ssh_connect_timeout_secs(resolved.connect_timeout_secs),
                    resolved.allow_exec_channel_proxy,
                )
                .await
                .map_err(|err| format!("SSH layer {} failed: {err}", index + 1))?,
            TransportLayerConfig::Ssh(resolved) => ssh_tunnels
                .start_tunnel_on_local_port(
                    &layer_id,
                    &connect_endpoint.host,
                    connect_endpoint.port,
                    &resolved.host,
                    resolved.port,
                    &resolved.user,
                    &resolved.password,
                    &resolved.key_path,
                    &resolved.key_passphrase,
                    resolved.use_ssh_agent,
                    &resolved.ssh_agent_sock_path,
                    &resolved.auth_method,
                    effective_ssh_connect_timeout_secs(resolved.connect_timeout_secs),
                    &target_endpoint.host,
                    target_endpoint.port,
                    is_last && resolved.expose_lan,
                    resolved.allow_exec_channel_proxy,
                    if is_last { final_ssh_local_port } else { None },
                )
                .await
                .map_err(|err| format!("SSH layer {} failed: {err}", index + 1))?,
            TransportLayerConfig::Proxy(proxy) => proxy_tunnels
                .start_tunnel(
                    &layer_id,
                    proxy.proxy_type,
                    &connect_endpoint.host,
                    connect_endpoint.port,
                    &proxy.username,
                    &proxy.password,
                    &target_endpoint.host,
                    target_endpoint.port,
                )
                .await
                .map_err(|err| format!("Proxy layer {} failed: {err}", index + 1))?,
            TransportLayerConfig::HttpTunnel(http) => http_tunnels
                .start_tunnel(
                    &layer_id,
                    &http.url,
                    &http.token,
                    http.connect_timeout_secs,
                    &target_endpoint.host,
                    target_endpoint.port,
                )
                .await
                .map_err(|err| format!("HTTP tunnel layer {} failed: {err}", index + 1))?,
            TransportLayerConfig::DbxGateway(gateway) => dbx_gateway
                .start_tunnel(&layer_id, &connect_endpoint.host, connect_endpoint.port, gateway)
                .await
                .map_err(|err| format!("DBX Gateway layer {} failed: {err}", index + 1))?,
        };

        final_local_port = local_port;
        next_connect_endpoint = Some(LayerEndpoint::localhost(local_port));
    }

    Ok(final_local_port)
}

pub async fn stop_transport_layers(
    connection_id: &str,
    layer_count: usize,
    ssh_tunnels: &TunnelManager,
    proxy_tunnels: &ProxyTunnelManager,
    http_tunnels: &HttpTunnelManager,
    dbx_gateway: &DbxGatewayManager,
) {
    for index in 0..layer_count {
        let layer_id = layer_id(connection_id, index);
        ssh_tunnels.stop_tunnel(&layer_id).await;
        proxy_tunnels.stop_tunnel(&layer_id).await;
        http_tunnels.stop_tunnel(&layer_id).await;
        dbx_gateway.stop_tunnel(&layer_id).await;
    }
}

fn validate_transport_layers(layers: &[TransportLayerConfig]) -> Result<(), String> {
    let gateway_count = layers.iter().filter(|layer| matches!(layer, TransportLayerConfig::DbxGateway(_))).count();
    if gateway_count > 1 {
        return Err("Only one DBX Gateway layer is allowed".to_string());
    }
    for (index, layer) in layers.iter().enumerate() {
        if matches!(layer, TransportLayerConfig::HttpTunnel(_)) && index != 0 {
            return Err("HTTP tunnel must be the first transport layer".to_string());
        }
        if matches!(layer, TransportLayerConfig::DbxGateway(_)) && index + 1 != layers.len() {
            return Err("DBX Gateway must be the final transport layer".to_string());
        }
    }
    Ok(())
}

fn layer_id(connection_id: &str, index: usize) -> String {
    format!("{connection_id}:transport:{index}")
}

fn effective_ssh_connect_timeout_secs(value: u64) -> u64 {
    if value == 0 {
        crate::models::connection::default_ssh_connect_timeout_secs()
    } else {
        value
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlannedLayerType {
    Ssh,
    Proxy,
    HttpTunnel,
    DbxGateway,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedTransportLayer {
    layer_type: PlannedLayerType,
    connect_host: String,
    connect_port: u16,
    remote_host: String,
    remote_port: u16,
}

#[cfg(test)]
fn plan_transport_layers(
    layers: &[TransportLayerConfig],
    remote_host: &str,
    remote_port: u16,
    local_ports: &[u16],
) -> Vec<PlannedTransportLayer> {
    plan_transport_layers_with_resolver(layers, remote_host, remote_port, local_ports, |ssh| ssh.clone())
}

/// Same as `plan_transport_layers`, but takes an explicit SSH-alias resolver
/// so tests can exercise `~/.ssh/config` resolution without touching the real
/// filesystem (mirrors `start_transport_layers`'s use of `resolve_ssh_layers`).
#[cfg(test)]
fn plan_transport_layers_with_resolver(
    layers: &[TransportLayerConfig],
    remote_host: &str,
    remote_port: u16,
    local_ports: &[u16],
    resolve: impl Fn(&crate::models::connection::SshTunnelConfig) -> crate::models::connection::SshTunnelConfig,
) -> Vec<PlannedTransportLayer> {
    let layers = resolve_ssh_layers(layers, resolve);
    let layers = layers.as_slice();
    let mut planned = Vec::new();
    let mut next_connect_endpoint: Option<(String, u16)> = None;
    for (index, layer) in layers.iter().enumerate() {
        let is_last = index + 1 == layers.len();
        let own_endpoint = layer_endpoint(layer);
        let (connect_host, connect_port) =
            next_connect_endpoint.clone().unwrap_or((own_endpoint.host, own_endpoint.port));
        let (target_host, target_port) = if is_last {
            (remote_host.to_string(), remote_port)
        } else {
            let next = layer_endpoint(&layers[index + 1]);
            (next.host, next.port)
        };
        let layer_type = match layer {
            TransportLayerConfig::Ssh(_) => PlannedLayerType::Ssh,
            TransportLayerConfig::Proxy(_) => PlannedLayerType::Proxy,
            TransportLayerConfig::HttpTunnel(_) => PlannedLayerType::HttpTunnel,
            TransportLayerConfig::DbxGateway(_) => PlannedLayerType::DbxGateway,
        };
        planned.push(PlannedTransportLayer {
            layer_type,
            connect_host,
            connect_port,
            remote_host: target_host,
            remote_port: target_port,
        });
        if let Some(local_port) = local_ports.get(index) {
            next_connect_endpoint = Some(("127.0.0.1".to_string(), *local_port));
        }
    }
    planned
}

#[cfg(test)]
mod tests {
    use super::{
        plan_transport_layers, plan_transport_layers_with_resolver, validate_transport_layers, PlannedLayerType,
        PlannedTransportLayer,
    };
    use crate::models::connection::{
        DbxGatewayConfig, HttpTunnelConfig, ProxyTunnelConfig, ProxyType, SshTunnelConfig, TransportLayerConfig,
    };

    fn ssh_layer(id: &str, host: &str, port: u16) -> TransportLayerConfig {
        TransportLayerConfig::Ssh(SshTunnelConfig {
            profile_id: String::new(),
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            host: host.to_string(),
            port,
            user: "user".to_string(),
            password: "secret".to_string(),
            key_path: String::new(),
            key_passphrase: String::new(),
            connect_timeout_secs: 5,
            expose_lan: false,
            use_ssh_agent: false,
            ssh_agent_sock_path: String::new(),
            auth_method: "password".to_string(),
            allow_exec_channel_proxy: false,
        })
    }

    fn proxy_layer(id: &str, host: &str, port: u16) -> TransportLayerConfig {
        TransportLayerConfig::Proxy(ProxyTunnelConfig {
            profile_id: String::new(),
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            proxy_type: ProxyType::Socks5,
            host: host.to_string(),
            port,
            username: String::new(),
            password: String::new(),
            test_target: None,
        })
    }

    fn http_tunnel_layer(id: &str, url: &str) -> TransportLayerConfig {
        TransportLayerConfig::HttpTunnel(HttpTunnelConfig {
            profile_id: String::new(),
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            url: url.to_string(),
            token: String::new(),
            connect_timeout_secs: 10,
        })
    }

    fn gateway_layer(id: &str) -> TransportLayerConfig {
        TransportLayerConfig::DbxGateway(DbxGatewayConfig {
            id: id.to_string(),
            name: String::new(),
            enabled: true,
            profile_id: String::new(),
            main_url: "wss://gateway.example.com/_dbx/client".to_string(),
            identity_id: "identity-1".to_string(),
            server_ca_pem: "ca".to_string(),
            server_spki_sha256: String::new(),
            connect_timeout_secs: 10,
            edge_id: "edge-prod-01".to_string(),
            target_id: "postgres-primary".to_string(),
            use_as_connection_info: true,
        })
    }

    #[test]
    fn mixed_transport_plan_routes_layers_in_configured_order() {
        let layers = vec![
            ssh_layer("ssh-a", "bastion-a", 22),
            proxy_layer("proxy", "proxy.internal", 1080),
            ssh_layer("ssh-b", "bastion-b", 2200),
        ];

        let planned = plan_transport_layers(&layers, "db.internal", 5432, &[41001, 41002, 41003]);

        assert_eq!(
            planned,
            vec![
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::Ssh,
                    connect_host: "bastion-a".to_string(),
                    connect_port: 22,
                    remote_host: "proxy.internal".to_string(),
                    remote_port: 1080,
                },
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::Proxy,
                    connect_host: "127.0.0.1".to_string(),
                    connect_port: 41001,
                    remote_host: "bastion-b".to_string(),
                    remote_port: 2200,
                },
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::Ssh,
                    connect_host: "127.0.0.1".to_string(),
                    connect_port: 41002,
                    remote_host: "db.internal".to_string(),
                    remote_port: 5432,
                },
            ]
        );
    }

    #[test]
    fn http_tunnel_must_be_outermost_layer() {
        let layers = vec![
            ssh_layer("ssh-a", "bastion-a", 22),
            http_tunnel_layer("http", "https://dbx.example.com/dbx_tunnel.php"),
        ];

        let err = validate_transport_layers(&layers).unwrap_err();

        assert!(err.contains("HTTP tunnel must be the first transport layer"));
    }

    #[test]
    fn single_ssh_hop_with_config_alias_connects_to_resolved_host_and_port() {
        // RF-001 regression: a single SSH layer whose `host` is a
        // `~/.ssh/config` alias must dial the resolved HostName/Port, not
        // the literal alias string.
        let layers = vec![ssh_layer("ssh-a", "myserver", 22)];

        let planned = plan_transport_layers_with_resolver(&layers, "db.internal", 5432, &[], |ssh| {
            assert_eq!(ssh.host, "myserver");
            let mut resolved = ssh.clone();
            resolved.host = "10.0.0.5".to_string();
            resolved.port = 2222;
            resolved
        });

        assert_eq!(
            planned,
            vec![PlannedTransportLayer {
                layer_type: PlannedLayerType::Ssh,
                connect_host: "10.0.0.5".to_string(),
                connect_port: 2222,
                remote_host: "db.internal".to_string(),
                remote_port: 5432,
            }]
        );
    }

    #[test]
    fn earlier_hop_forwards_to_resolved_alias_of_next_ssh_hop() {
        // RF-002 regression: when hop N+1 is an SSH layer addressed by a
        // config alias, hop N's forward target must be the resolved
        // HostName/Port, not the literal alias (which the remote host at
        // hop N cannot resolve).
        let layers = vec![ssh_layer("ssh-a", "bastion-a", 22), ssh_layer("ssh-b", "myserver", 22)];

        let planned = plan_transport_layers_with_resolver(&layers, "db.internal", 5432, &[41001], |ssh| {
            let mut resolved = ssh.clone();
            if ssh.host == "myserver" {
                resolved.host = "10.0.0.5".to_string();
                resolved.port = 2222;
            }
            resolved
        });

        assert_eq!(
            planned,
            vec![
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::Ssh,
                    connect_host: "bastion-a".to_string(),
                    connect_port: 22,
                    remote_host: "10.0.0.5".to_string(),
                    remote_port: 2222,
                },
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::Ssh,
                    connect_host: "127.0.0.1".to_string(),
                    connect_port: 41001,
                    remote_host: "db.internal".to_string(),
                    remote_port: 5432,
                },
            ]
        );
    }

    #[test]
    fn http_tunnel_first_layer_targets_next_layer() {
        let layers = vec![
            http_tunnel_layer("http", "https://dbx.example.com/dbx_tunnel.php"),
            ssh_layer("ssh-a", "bastion-a", 22),
        ];

        let planned = plan_transport_layers(&layers, "db.internal", 5432, &[41001, 41002]);

        assert_eq!(
            planned,
            vec![
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::HttpTunnel,
                    connect_host: "".to_string(),
                    connect_port: 0,
                    remote_host: "bastion-a".to_string(),
                    remote_port: 22,
                },
                PlannedTransportLayer {
                    layer_type: PlannedLayerType::Ssh,
                    connect_host: "127.0.0.1".to_string(),
                    connect_port: 41001,
                    remote_host: "db.internal".to_string(),
                    remote_port: 5432,
                },
            ]
        );
    }

    #[test]
    fn gateway_is_the_final_layer_and_dials_through_the_previous_hop() {
        let layers = vec![ssh_layer("ssh-a", "bastion-a", 22), gateway_layer("gateway")];

        validate_transport_layers(&layers).unwrap();
        let planned = plan_transport_layers(&layers, "db.internal", 5432, &[41001, 41002]);

        assert_eq!(planned[0].remote_host, "gateway.example.com");
        assert_eq!(planned[0].remote_port, 443);
        assert_eq!(planned[1].layer_type, PlannedLayerType::DbxGateway);
        assert_eq!(planned[1].connect_host, "127.0.0.1");
        assert_eq!(planned[1].connect_port, 41001);
    }

    #[test]
    fn gateway_before_another_layer_is_rejected() {
        let error =
            validate_transport_layers(&[gateway_layer("gateway"), ssh_layer("ssh-a", "bastion-a", 22)]).unwrap_err();
        assert!(error.contains("final"));
    }
}
