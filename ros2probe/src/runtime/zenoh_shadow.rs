use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{anyhow, bail};
use bytes::Bytes;
use log::debug;
use zenoh::{pubsub::Subscriber, sample::SampleKind};

use crate::{
    capture::{TransportProtocol, ZenohCapturePorts},
    protocols::zenoh::{
        ZenohRosTopicSample, parse_rmw_zenoh_attachment_identity, parse_ros_topic_sample_keyexpr,
    },
};

use super::zenoh_discover::{LocalZenohEndpoint, local_client_config, local_client_endpoints};

const SHADOW_RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct ZenohShadowSample {
    pub sample: ZenohRosTopicSample,
    pub observed_at: SystemTime,
}

struct ZenohShadowConnection {
    session: zenoh::Session,
    subscribers: BTreeMap<String, Subscriber<()>>,
}

pub(super) struct ZenohShadow {
    connections: BTreeMap<LocalZenohEndpoint, ZenohShadowConnection>,
    target_keyexprs: BTreeSet<String>,
    target_endpoints: BTreeSet<LocalZenohEndpoint>,
    retry_at: Option<Instant>,
    sample_tx: mpsc::SyncSender<ZenohShadowSample>,
}

impl ZenohShadow {
    pub fn new(sample_tx: mpsc::SyncSender<ZenohShadowSample>) -> Self {
        Self {
            connections: BTreeMap::new(),
            target_keyexprs: BTreeSet::new(),
            target_endpoints: BTreeSet::new(),
            retry_at: None,
            sample_tx,
        }
    }

    pub async fn sync(
        &mut self,
        target_keyexprs: BTreeSet<String>,
        zenoh_ports: &ZenohCapturePorts,
        observed_transports: &HashSet<TransportProtocol>,
    ) -> anyhow::Result<()> {
        let target_endpoints = if target_keyexprs.is_empty() {
            BTreeSet::new()
        } else {
            local_client_endpoints(zenoh_ports, observed_transports)
                .into_iter()
                .collect::<BTreeSet<_>>()
        };
        let target_changed =
            target_keyexprs != self.target_keyexprs || target_endpoints != self.target_endpoints;
        if target_changed {
            self.target_keyexprs = target_keyexprs;
            self.target_endpoints = target_endpoints;
            self.retry_at = None;
        } else if self
            .retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return Ok(());
        }

        if self.target_keyexprs.is_empty() {
            self.close().await?;
            return Ok(());
        }
        if self.target_endpoints.is_empty() {
            bail!("no Zenoh TCP or UDP transport port is configured for shadow subscriber");
        }

        if !target_changed
            && self.retry_at.is_none()
            && self.connections.keys().eq(self.target_endpoints.iter())
            && self.connections.values().all(|connection| {
                connection.subscribers.len() == self.target_keyexprs.len()
                    && connection
                        .subscribers
                        .keys()
                        .all(|keyexpr| self.target_keyexprs.contains(keyexpr))
            })
        {
            return Ok(());
        }

        match self.reconcile().await {
            Ok(incomplete) => {
                self.retry_at = incomplete.then(|| Instant::now() + SHADOW_RETRY_INTERVAL);
                Ok(())
            }
            Err(err) => {
                self.retry_at = Some(Instant::now() + SHADOW_RETRY_INTERVAL);
                Err(err)
            }
        }
    }

    /// Returns `true` when at least one endpoint worked but another endpoint
    /// remains unavailable and should be retried.
    async fn reconcile(&mut self) -> anyhow::Result<bool> {
        let obsolete_endpoints = self
            .connections
            .keys()
            .filter(|endpoint| !self.target_endpoints.contains(endpoint))
            .copied()
            .collect::<Vec<_>>();
        for endpoint in obsolete_endpoints {
            if let Some(connection) = self.connections.remove(&endpoint) {
                close_connection(endpoint, connection).await?;
            }
        }

        for (endpoint, connection) in &mut self.connections {
            let obsolete_keyexprs = connection
                .subscribers
                .keys()
                .filter(|keyexpr| !self.target_keyexprs.contains(*keyexpr))
                .cloned()
                .collect::<Vec<_>>();
            for keyexpr in obsolete_keyexprs {
                if let Some(subscriber) = connection.subscribers.remove(&keyexpr) {
                    subscriber.undeclare().await.map_err(|err| {
                        anyhow!("undeclare Zenoh shadow subscriber {keyexpr} on {endpoint}: {err}")
                    })?;
                }
            }
        }

        let mut errors = Vec::new();
        for endpoint in self.target_endpoints.iter().copied() {
            if self.connections.contains_key(&endpoint) {
                continue;
            }
            let config = match local_client_config(endpoint) {
                Ok(config) => config,
                Err(err) => {
                    errors.push(format!("{endpoint}: {err:#}"));
                    continue;
                }
            };
            match zenoh::open(config).await {
                Ok(session) => {
                    self.connections.insert(
                        endpoint,
                        ZenohShadowConnection {
                            session,
                            subscribers: BTreeMap::new(),
                        },
                    );
                }
                Err(err) => errors.push(format!("{endpoint}: {err}")),
            }
        }

        for (endpoint, connection) in &mut self.connections {
            let missing = self
                .target_keyexprs
                .iter()
                .filter(|keyexpr| !connection.subscribers.contains_key(*keyexpr))
                .cloned()
                .collect::<Vec<_>>();
            for keyexpr in missing {
                let sample_tx = self.sample_tx.clone();
                match connection
                    .session
                    .declare_subscriber(keyexpr.as_str())
                    .callback(move |sample| {
                        if let Some(sample) = decode_shadow_sample(sample) {
                            let _ = sample_tx.try_send(sample);
                        }
                    })
                    .await
                {
                    Ok(subscriber) => {
                        connection.subscribers.insert(keyexpr, subscriber);
                    }
                    Err(err) => errors.push(format!(
                        "{endpoint}: declare Zenoh shadow subscriber {keyexpr}: {err}"
                    )),
                }
            }
        }

        let has_complete_connection = self.connections.values().any(|connection| {
            connection.subscribers.len() == self.target_keyexprs.len()
                && self
                    .target_keyexprs
                    .iter()
                    .all(|keyexpr| connection.subscribers.contains_key(keyexpr))
        });
        if !has_complete_connection {
            let detail = if errors.is_empty() {
                "no endpoint completed its subscriptions".to_string()
            } else {
                errors.join("; ")
            };
            bail!("open Zenoh shadow subscriber session: {detail}");
        }

        if !errors.is_empty() {
            debug!(
                "Zenoh shadow subscriber is active with partial endpoint failures: {}",
                errors.join("; ")
            );
        }
        Ok(!errors.is_empty())
    }

    pub async fn close(&mut self) -> anyhow::Result<()> {
        self.target_keyexprs.clear();
        self.target_endpoints.clear();
        self.retry_at = None;
        let connections = std::mem::take(&mut self.connections);
        let mut errors = Vec::new();
        for (endpoint, connection) in connections {
            if let Err(err) = close_connection(endpoint, connection).await {
                errors.push(format!("{err:#}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("close Zenoh shadow sessions: {}", errors.join("; "))
        }
    }
}

async fn close_connection(
    endpoint: LocalZenohEndpoint,
    mut connection: ZenohShadowConnection,
) -> anyhow::Result<()> {
    connection.subscribers.clear();
    connection
        .session
        .close()
        .await
        .map_err(|err| anyhow!("close Zenoh shadow session for {endpoint}: {err}"))
}

fn decode_shadow_sample(sample: zenoh::sample::Sample) -> Option<ZenohShadowSample> {
    if sample.kind() != SampleKind::Put {
        return None;
    }

    let payload_len = sample.payload().len();
    let payload = Bytes::copy_from_slice(sample.payload().to_bytes().as_ref());
    let attachment = sample.attachment().map(|bytes| bytes.to_bytes());
    let attachment_len = attachment.as_ref().map(|bytes| bytes.len());
    let identity = attachment
        .as_deref()
        .and_then(parse_rmw_zenoh_attachment_identity);
    let mut sample = parse_ros_topic_sample_keyexpr(
        sample.key_expr().as_str(),
        payload,
        payload_len,
        attachment_len,
    )
    .ok()?;
    sample.identity = identity;

    Some(ZenohShadowSample {
        sample,
        observed_at: SystemTime::now(),
    })
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, UdpSocket};

    use ros2probe_common::TopicGid;

    use super::*;

    fn unused_port(protocol: TransportProtocol) -> u16 {
        match protocol {
            TransportProtocol::Tcp => {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                listener.local_addr().unwrap().port()
            }
            TransportProtocol::Udp => {
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                socket.local_addr().unwrap().port()
            }
        }
    }

    async fn shadow_client_receives_raw_payload(protocol: TransportProtocol) {
        let port = unused_port(protocol);
        let scheme = match protocol {
            TransportProtocol::Tcp => "tcp",
            TransportProtocol::Udp => "udp",
        };

        let mut peer_config = zenoh::Config::default();
        for (key, value) in [
            ("mode", "\"peer\"".to_string()),
            ("connect/endpoints", "[]".to_string()),
            (
                "listen/endpoints",
                format!("[\"{scheme}/127.0.0.1:{port}\"]"),
            ),
            ("scouting/multicast/enabled", "false".to_string()),
            ("scouting/gossip/enabled", "false".to_string()),
        ] {
            peer_config.insert_json5(key, &value).unwrap();
        }
        let peer = zenoh::open(peer_config).await.unwrap();

        let (sample_tx, sample_rx) = mpsc::sync_channel(16);
        let mut shadow = ZenohShadow::new(sample_tx);
        let keyexpr = "0/chatter/std_msgs::msg::dds_::String_/RIHS01_abcd";
        shadow
            .sync(
                BTreeSet::from([keyexpr.to_string()]),
                &ZenohCapturePorts::from_transport_ports([port]),
                &HashSet::from([protocol]),
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let source_gid = [17u8; 16];
        let mut attachment = Vec::new();
        attachment.extend_from_slice(&42i64.to_le_bytes());
        attachment.extend_from_slice(&1_234_567i64.to_le_bytes());
        attachment.push(source_gid.len() as u8);
        attachment.extend_from_slice(&source_gid);
        peer.put(keyexpr, b"hello".as_slice())
            .attachment(attachment)
            .await
            .unwrap();

        let received = sample_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(received.sample.topic_name, "/chatter");
        assert_eq!(received.sample.payload.as_ref(), b"hello");
        assert_eq!(received.sample.payload_len, 5);
        assert_eq!(
            received.sample.identity,
            Some(crate::protocols::zenoh::ZenohRosSampleIdentity {
                source_gid: TopicGid::new(source_gid),
                sequence_number: 42,
            })
        );

        shadow.close().await.unwrap();
        peer.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tcp_shadow_client_receives_raw_payload_and_rmw_identity() {
        shadow_client_receives_raw_payload(TransportProtocol::Tcp).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn udp_shadow_client_receives_raw_payload_and_rmw_identity() {
        shadow_client_receives_raw_payload(TransportProtocol::Udp).await;
    }
}
