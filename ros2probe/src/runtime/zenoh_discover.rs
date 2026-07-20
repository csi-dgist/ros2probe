use std::{
    collections::{BTreeSet, HashSet},
    env, fmt,
    time::Duration,
};

use anyhow::{Context, anyhow, bail};

use crate::{
    capture::{TransportProtocol, ZenohCapturePorts},
    command::protocol::DiscoverRequest,
};

const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct LocalZenohEndpoint {
    pub protocol: TransportProtocol,
    pub port: u16,
}

impl LocalZenohEndpoint {
    fn locator(self) -> String {
        format!("{}/127.0.0.1:{}", self.scheme(), self.port)
    }

    fn scheme(self) -> &'static str {
        match self.protocol {
            TransportProtocol::Tcp => "tcp",
            TransportProtocol::Udp => "udp",
        }
    }
}

impl fmt::Display for LocalZenohEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.locator())
    }
}

pub(super) struct ZenohLivelinessSnapshot {
    pub tokens: Vec<String>,
    pub queried_keyexprs: Vec<String>,
    pub successful_endpoints: Vec<LocalZenohEndpoint>,
    pub failed_endpoints: Vec<(LocalZenohEndpoint, String)>,
}

pub(super) async fn liveliness_get(
    request: &DiscoverRequest,
    zenoh_ports: &ZenohCapturePorts,
    observed_transports: &HashSet<TransportProtocol>,
) -> anyhow::Result<ZenohLivelinessSnapshot> {
    let domain_id = ros_domain_id(request)?;
    // Query only the requested ROS domain. The previous global fallback was
    // a strict superset of this expression, doubled the query latency, and
    // imported nodes from unrelated ROS_DOMAIN_ID values into the graph.
    let query_keyexprs = [format!("@ros2_lv/{domain_id}/**")];
    let endpoints = discovery_client_endpoints(zenoh_ports, observed_transports);
    if endpoints.is_empty() {
        bail!("no Zenoh TCP or UDP transport port is configured");
    }

    let mut tokens = BTreeSet::new();
    let mut successful_endpoints = Vec::new();
    let mut failed_endpoints = Vec::new();
    let attempts = endpoints
        .into_iter()
        .map(|endpoint| {
            let query_keyexprs = query_keyexprs.clone();
            let task =
                tokio::spawn(
                    async move { endpoint_liveliness_get(endpoint, &query_keyexprs).await },
                );
            (endpoint, task)
        })
        .collect::<Vec<_>>();
    for (endpoint, attempt) in attempts {
        match attempt.await {
            Ok(Ok(endpoint_tokens)) => {
                successful_endpoints.push(endpoint);
                tokens.extend(endpoint_tokens);
            }
            Ok(Err(err)) => failed_endpoints.push((endpoint, format!("{err:#}"))),
            Err(err) => failed_endpoints.push((endpoint, format!("discovery task failed: {err}"))),
        }
    }

    if successful_endpoints.is_empty() {
        let failures = failed_endpoints
            .iter()
            .map(|(endpoint, error)| format!("{endpoint}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("unable to query any local Zenoh endpoint: {failures}");
    }

    Ok(ZenohLivelinessSnapshot {
        tokens: tokens.into_iter().collect(),
        queried_keyexprs: query_keyexprs.into_iter().collect(),
        successful_endpoints,
        failed_endpoints,
    })
}

async fn endpoint_liveliness_get(
    endpoint: LocalZenohEndpoint,
    query_keyexprs: &[String],
) -> anyhow::Result<Vec<String>> {
    let session = zenoh::open(local_client_config(endpoint)?)
        .await
        .map_err(|err| anyhow!("open Zenoh discovery session for {endpoint}: {err}"))?;
    let mut tokens = BTreeSet::new();

    let query_result = async {
        for keyexpr in query_keyexprs {
            for token in liveliness_get_keyexpr(&session, keyexpr).await? {
                tokens.insert(token);
            }
        }

        Ok::<(), anyhow::Error>(())
    }
    .await;
    let close_result = session
        .close()
        .await
        .map_err(|err| anyhow!("close Zenoh discovery session for {endpoint}: {err}"));
    query_result?;
    close_result?;

    Ok(tokens.into_iter().collect())
}

async fn liveliness_get_keyexpr(
    session: &zenoh::Session,
    keyexpr: &str,
) -> anyhow::Result<Vec<String>> {
    let replies: zenoh::handlers::FifoChannelHandler<zenoh::query::Reply> = session
        .liveliness()
        .get(keyexpr)
        .timeout(DEFAULT_QUERY_TIMEOUT)
        .await
        .map_err(|err| anyhow!("query Zenoh liveliness tokens for {keyexpr}: {err}"))?;

    let mut tokens = Vec::new();
    while let Ok(reply) = replies.recv_async().await {
        match reply.result() {
            Ok(sample) => tokens.push(sample.key_expr().as_str().to_string()),
            Err(err) => {
                bail!("Zenoh liveliness query returned error: {:?}", err.payload());
            }
        }
    }

    Ok(tokens)
}

pub(super) fn local_client_endpoints(
    zenoh_ports: &ZenohCapturePorts,
    observed_transports: &HashSet<TransportProtocol>,
) -> Vec<LocalZenohEndpoint> {
    let include_tcp =
        observed_transports.is_empty() || observed_transports.contains(&TransportProtocol::Tcp);
    let include_udp =
        observed_transports.is_empty() || observed_transports.contains(&TransportProtocol::Udp);
    let mut endpoints = Vec::new();

    if include_tcp {
        endpoints.extend(
            zenoh_ports
                .tcp_ports()
                .iter()
                .map(|port| LocalZenohEndpoint {
                    protocol: TransportProtocol::Tcp,
                    port: *port,
                }),
        );
    }
    if include_udp {
        endpoints.extend(
            zenoh_ports
                .udp_ports()
                .iter()
                .map(|port| LocalZenohEndpoint {
                    protocol: TransportProtocol::Udp,
                    port: *port,
                }),
        );
    }

    endpoints
}

fn discovery_client_endpoints(
    zenoh_ports: &ZenohCapturePorts,
    observed_transports: &HashSet<TransportProtocol>,
) -> Vec<LocalZenohEndpoint> {
    let mut endpoints = local_client_endpoints(zenoh_ports, observed_transports);
    if observed_transports.is_empty() {
        return endpoints;
    }

    let unobserved_transports = [TransportProtocol::Tcp, TransportProtocol::Udp]
        .into_iter()
        .filter(|protocol| !observed_transports.contains(protocol))
        .collect::<HashSet<_>>();
    if unobserved_transports.is_empty() {
        return endpoints;
    }
    endpoints.extend(local_client_endpoints(zenoh_ports, &unobserved_transports));
    endpoints
}

pub(super) fn local_client_config(endpoint: LocalZenohEndpoint) -> anyhow::Result<zenoh::Config> {
    let endpoints =
        serde_json::to_string(&[endpoint.locator()]).context("serialize local Zenoh endpoint")?;
    let mut config = zenoh::Config::default();
    for (key, value) in [
        ("mode", "\"client\""),
        ("connect/endpoints", endpoints.as_str()),
        ("connect/timeout_ms", "0"),
        ("connect/exit_on_failure", "true"),
        ("listen/endpoints", "[]"),
        ("scouting/multicast/enabled", "false"),
        ("scouting/gossip/enabled", "false"),
        ("transport/shared_memory/enabled", "false"),
        (
            "transport/shared_memory/transport_optimization/enabled",
            "false",
        ),
    ] {
        config
            .insert_json5(key, value)
            .map_err(|err| anyhow!("configure Zenoh local client key {key}: {err}"))?;
    }
    Ok(config)
}

fn ros_domain_id(request: &DiscoverRequest) -> anyhow::Result<u32> {
    match request
        .ros_domain_id
        .clone()
        .or_else(|| nonempty_env("ROS_DOMAIN_ID"))
    {
        None => Ok(0),
        Some(value) => value
            .trim()
            .parse::<u32>()
            .with_context(|| format!("parse ROS_DOMAIN_ID={value:?}")),
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;

    use super::*;

    fn endpoint(protocol: TransportProtocol, port: u16) -> LocalZenohEndpoint {
        LocalZenohEndpoint { protocol, port }
    }

    #[test]
    fn local_tcp_client_uses_runtime_capture_port_and_disables_shm() {
        let config = local_client_config(endpoint(TransportProtocol::Tcp, 7447)).unwrap();

        assert_eq!(config.get_json("mode").unwrap(), "\"client\"");
        assert_eq!(
            config.get_json("connect/endpoints").unwrap(),
            "[\"tcp/127.0.0.1:7447\"]"
        );
        assert_eq!(config.get_json("connect/timeout_ms").unwrap(), "0");
        assert_eq!(config.get_json("connect/exit_on_failure").unwrap(), "true");
        assert_eq!(config.get_json("listen/endpoints").unwrap(), "[]");
        assert_eq!(
            config.get_json("scouting/multicast/enabled").unwrap(),
            "false"
        );
        assert_eq!(config.get_json("scouting/gossip/enabled").unwrap(), "false");
        assert_eq!(
            config.get_json("transport/shared_memory/enabled").unwrap(),
            "false"
        );
        assert_eq!(
            config
                .get_json("transport/shared_memory/transport_optimization/enabled")
                .unwrap(),
            "false"
        );
    }

    #[test]
    fn local_udp_client_uses_unicast_loopback_endpoint() {
        let config = local_client_config(endpoint(TransportProtocol::Udp, 7447)).unwrap();

        assert_eq!(
            config.get_json("connect/endpoints").unwrap(),
            "[\"udp/127.0.0.1:7447\"]"
        );
    }

    #[test]
    fn observed_transport_limits_endpoint_candidates() {
        let ports = ZenohCapturePorts::from_transport_ports([7447, 8447]);

        assert_eq!(
            local_client_endpoints(&ports, &HashSet::from([TransportProtocol::Udp])),
            vec![
                endpoint(TransportProtocol::Udp, 7447),
                endpoint(TransportProtocol::Udp, 8447),
            ]
        );
        assert_eq!(
            local_client_endpoints(&ports, &HashSet::new()),
            vec![
                endpoint(TransportProtocol::Tcp, 7447),
                endpoint(TransportProtocol::Tcp, 8447),
                endpoint(TransportProtocol::Udp, 7447),
                endpoint(TransportProtocol::Udp, 8447),
            ]
        );
    }

    #[test]
    fn discovery_tries_observed_transport_first_then_falls_back() {
        let ports = ZenohCapturePorts::from_transport_ports([7447]);

        assert_eq!(
            discovery_client_endpoints(&ports, &HashSet::from([TransportProtocol::Udp])),
            vec![
                endpoint(TransportProtocol::Udp, 7447),
                endpoint(TransportProtocol::Tcp, 7447),
            ]
        );
        assert_eq!(
            discovery_client_endpoints(
                &ports,
                &HashSet::from([TransportProtocol::Tcp, TransportProtocol::Udp]),
            ),
            vec![
                endpoint(TransportProtocol::Tcp, 7447),
                endpoint(TransportProtocol::Udp, 7447),
            ]
        );
    }

    #[test]
    fn request_ros_domain_id_takes_precedence() {
        let request = DiscoverRequest {
            ros_domain_id: Some("42".to_string()),
            ..DiscoverRequest::default()
        };

        assert_eq!(ros_domain_id(&request).unwrap(), 42);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_liveliness_query_imports_existing_token() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);

        let mut peer_config = zenoh::Config::default();
        for (key, value) in [
            ("mode", "\"peer\"".to_string()),
            ("connect/endpoints", "[]".to_string()),
            ("listen/endpoints", format!("[\"udp/127.0.0.1:{port}\"]")),
            ("scouting/multicast/enabled", "false".to_string()),
            ("scouting/gossip/enabled", "false".to_string()),
        ] {
            peer_config.insert_json5(key, &value).unwrap();
        }
        let peer = zenoh::open(peer_config).await.unwrap();
        let keyexpr = "@ros2_lv/0/udp_discovery_test";
        let token = peer.liveliness().declare_token(keyexpr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let request = DiscoverRequest {
            ros_domain_id: Some("0".to_string()),
            ..DiscoverRequest::default()
        };
        let snapshot = liveliness_get(
            &request,
            &ZenohCapturePorts::from_transport_ports([port]),
            &HashSet::from([TransportProtocol::Udp]),
        )
        .await
        .unwrap();

        assert!(snapshot.tokens.iter().any(|token| token == keyexpr));
        assert_eq!(
            snapshot.successful_endpoints,
            vec![endpoint(TransportProtocol::Udp, port)]
        );
        assert_eq!(snapshot.failed_endpoints.len(), 1);
        assert_eq!(
            snapshot.failed_endpoints[0].0,
            endpoint(TransportProtocol::Tcp, port)
        );

        token.undeclare().await.unwrap();
        peer.close().await.unwrap();
    }
}
