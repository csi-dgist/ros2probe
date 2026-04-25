use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, SystemTime},
};

use ros2probe_common::TopicGid;

use crate::discovery::{
    DiscoveredEndpoint, DiscoveredParticipant, DiscoverySample, DurationValue, DurabilityQos,
    LivelinessQos, Locator, ReliabilityQos,
};

#[derive(Debug)]
pub struct DiscoveryTable {
    participants: BTreeMap<TopicGid, ParticipantEntry>,
    publications: BTreeMap<TopicGid, EndpointEntry>,
    subscriptions: BTreeMap<TopicGid, EndpointEntry>,
    default_endpoint_ttl: Duration,
}

#[derive(Debug, Default)]
pub struct NodeTable {
    nodes: BTreeMap<NodeKey, NodeEntry>,
    writer_index: BTreeMap<TopicGid, NodeKey>,
    reader_index: BTreeMap<TopicGid, NodeKey>,
    participant_index: BTreeMap<TopicGid, BTreeSet<NodeKey>>,
}

#[derive(Clone, Debug)]
pub struct ParticipantEntry {
    pub gid: TopicGid,
    pub lease_duration: Option<DurationValue>,
    pub default_unicast_locators: Vec<Locator>,
    pub metatraffic_unicast_locators: Vec<Locator>,
    pub metatraffic_multicast_locators: Vec<Locator>,
    pub first_seen_at: SystemTime,
    pub last_seen_at: SystemTime,
    pub expires_at: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct EndpointEntry {
    pub endpoint_gid: TopicGid,
    pub participant_gid: Option<TopicGid>,
    pub topic_name: Option<String>,
    pub type_name: Option<String>,
    pub reliability: Option<ReliabilityQos>,
    pub durability: Option<DurabilityQos>,
    pub deadline: Option<DurationValue>,
    pub latency_budget: Option<DurationValue>,
    pub lifespan: Option<DurationValue>,
    pub liveliness: Option<LivelinessQos>,
    pub unicast_locators: Vec<Locator>,
    pub multicast_locators: Vec<Locator>,
    pub first_seen_at: SystemTime,
    pub last_seen_at: SystemTime,
    pub expires_at: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryChange {
    Inserted,
    Updated,
    Noop,
    /// A participant was gracefully disposed; its GID is carried for NodeTable cleanup.
    RemovedParticipant(TopicGid),
    /// An individual endpoint was gracefully disposed.
    RemovedEndpoint,
}

#[derive(Clone, Debug)]
pub struct TopicView {
    pub topic_name: String,
    pub type_names: Vec<String>,
    pub publisher_count: usize,
    pub subscription_count: usize,
    pub publisher_gids: Vec<TopicGid>,
    pub subscription_gids: Vec<TopicGid>,
}

#[derive(Clone, Debug, Default)]
pub struct ExpireStats {
    pub participants_removed: usize,
    pub publications_removed: usize,
    pub subscriptions_removed: usize,
    pub removed_participant_gids: Vec<TopicGid>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NodeKey {
    pub participant_gid: TopicGid,
    pub node_namespace: String,
    pub node_name: String,
}

#[derive(Clone, Debug)]
pub struct NodeEntry {
    pub key: NodeKey,
    pub writer_gids: BTreeSet<TopicGid>,
    pub reader_gids: BTreeSet<TopicGid>,
    pub first_seen_at: SystemTime,
    pub last_seen_at: SystemTime,
}

#[derive(Clone, Debug)]
pub struct NodeSample {
    pub participant_gid: TopicGid,
    pub node_namespace: String,
    pub node_name: String,
    pub writer_gids: Vec<TopicGid>,
    pub reader_gids: Vec<TopicGid>,
}

impl DiscoveryTable {
    pub fn new(default_endpoint_ttl: Duration) -> Self {
        Self {
            participants: BTreeMap::new(),
            publications: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            default_endpoint_ttl,
        }
    }

    pub fn apply_sample(&mut self, sample: DiscoverySample, observed_at: SystemTime) -> DiscoveryChange {
        match sample {
            DiscoverySample::Participant(participant) => self.upsert_participant(participant, observed_at),
            DiscoverySample::Publication(endpoint) => {
                let expires_at = self.endpoint_expires_at(endpoint.participant_gid, observed_at);
                Self::upsert_endpoint(&mut self.publications, endpoint, observed_at, expires_at)
            }
            DiscoverySample::Subscription(endpoint) => {
                let expires_at = self.endpoint_expires_at(endpoint.participant_gid, observed_at);
                Self::upsert_endpoint(&mut self.subscriptions, endpoint, observed_at, expires_at)
            }
            DiscoverySample::UnknownBuiltin(_) => DiscoveryChange::Noop,
            DiscoverySample::Disposed { kind, gid } => self.apply_disposal(kind, gid),
        }
    }

    fn apply_disposal(
        &mut self,
        kind: crate::discovery::DiscoveryBuiltinKind,
        gid: TopicGid,
    ) -> DiscoveryChange {
        use crate::discovery::DiscoveryBuiltinKind;
        match kind {
            DiscoveryBuiltinKind::Participant => {
                if self.participants.remove(&gid).is_some() {
                    self.publications.retain(|_, e| e.participant_gid != Some(gid));
                    self.subscriptions.retain(|_, e| e.participant_gid != Some(gid));
                    DiscoveryChange::RemovedParticipant(gid)
                } else {
                    DiscoveryChange::Noop
                }
            }
            DiscoveryBuiltinKind::Publication => {
                if self.publications.remove(&gid).is_some() {
                    DiscoveryChange::RemovedEndpoint
                } else {
                    DiscoveryChange::Noop
                }
            }
            DiscoveryBuiltinKind::Subscription => {
                if self.subscriptions.remove(&gid).is_some() {
                    DiscoveryChange::RemovedEndpoint
                } else {
                    DiscoveryChange::Noop
                }
            }
            _ => DiscoveryChange::Noop,
        }
    }

    pub fn expire_stale(&mut self, now: SystemTime) -> ExpireStats {
        let mut stats = ExpireStats::default();

        let expired_participant_gids = self
            .participants
            .iter()
            .filter_map(|(gid, entry)| is_expired(entry.expires_at, now).then_some(*gid))
            .collect::<Vec<_>>();
        for gid in &expired_participant_gids {
            self.participants.remove(gid);
            stats.participants_removed += 1;
        }
        stats.removed_participant_gids = expired_participant_gids;

        stats.publications_removed += expire_endpoints(&mut self.publications, now);
        stats.subscriptions_removed += expire_endpoints(&mut self.subscriptions, now);

        stats
    }

    pub fn participant(&self, gid: &TopicGid) -> Option<&ParticipantEntry> {
        self.participants.get(gid)
    }

    pub fn publication(&self, gid: &TopicGid) -> Option<&EndpointEntry> {
        self.publications.get(gid)
    }

    pub fn subscription(&self, gid: &TopicGid) -> Option<&EndpointEntry> {
        self.subscriptions.get(gid)
    }

    pub fn publications(&self) -> &BTreeMap<TopicGid, EndpointEntry> {
        &self.publications
    }

    pub fn subscriptions(&self) -> &BTreeMap<TopicGid, EndpointEntry> {
        &self.subscriptions
    }

    pub fn topics(&self) -> Vec<TopicView> {
        let mut by_topic = BTreeMap::<String, TopicAccumulator>::new();

        for (gid, publication) in &self.publications {
            let Some(topic_name) = &publication.topic_name else {
                continue;
            };
            let entry = by_topic.entry(topic_name.clone()).or_default();
            entry.publisher_gids.push(*gid);
            if let Some(type_name) = &publication.type_name {
                entry.type_names.insert(type_name.clone());
            }
        }

        for (gid, subscription) in &self.subscriptions {
            let Some(topic_name) = &subscription.topic_name else {
                continue;
            };
            let entry = by_topic.entry(topic_name.clone()).or_default();
            entry.subscription_gids.push(*gid);
            if let Some(type_name) = &subscription.type_name {
                entry.type_names.insert(type_name.clone());
            }
        }

        by_topic
            .into_iter()
            .map(|(topic_name, acc)| TopicView {
                topic_name,
                type_names: acc.type_names.into_iter().collect(),
                publisher_count: acc.publisher_gids.len(),
                subscription_count: acc.subscription_gids.len(),
                publisher_gids: acc.publisher_gids,
                subscription_gids: acc.subscription_gids,
            })
            .collect()
    }

    pub fn topic(&self, topic_name: &str) -> Option<TopicView> {
        let mut acc = TopicAccumulator::default();
        let mut found = false;
        for (gid, pub_) in &self.publications {
            if pub_.topic_name.as_deref() == Some(topic_name) {
                found = true;
                acc.publisher_gids.push(*gid);
                if let Some(t) = &pub_.type_name {
                    acc.type_names.insert(t.clone());
                }
            }
        }
        for (gid, sub) in &self.subscriptions {
            if sub.topic_name.as_deref() == Some(topic_name) {
                found = true;
                acc.subscription_gids.push(*gid);
                if let Some(t) = &sub.type_name {
                    acc.type_names.insert(t.clone());
                }
            }
        }
        found.then(|| TopicView {
            topic_name: topic_name.to_string(),
            type_names: acc.type_names.into_iter().collect(),
            publisher_count: acc.publisher_gids.len(),
            subscription_count: acc.subscription_gids.len(),
            publisher_gids: acc.publisher_gids,
            subscription_gids: acc.subscription_gids,
        })
    }

    fn upsert_participant(
        &mut self,
        participant: DiscoveredParticipant,
        observed_at: SystemTime,
    ) -> DiscoveryChange {
        let Some(gid) = participant.guid else {
            return DiscoveryChange::Noop;
        };
        let expires_at = participant
            .lease_duration
            .and_then(duration_value_to_duration)
            .and_then(|lease| observed_at.checked_add(lease));

        match self.participants.get_mut(&gid) {
            Some(entry) => {
                // NOTE: always return `Updated` even when the advertised
                // participant fields are identical to what we already have.
                // The caller uses this signal to trigger `refresh_from_discovery`,
                // which rebuilds `CommandState` from *all* sources —
                // `discovery_table`, `node_table` (mutated orphan via
                // `/ros_discovery_info` DATA), and `remote_participants`
                // (mutated when SPDP is first seen from a non-local IP).
                // Returning `Noop` on unchanged SPDP heartbeats would leave
                // those orphan mutations stuck out of `CommandState` until
                // the next SEDP change, which some DDS implementations never
                // emit after the initial endpoint announcement.
                entry.lease_duration = participant.lease_duration;
                entry.default_unicast_locators = participant.default_unicast_locators;
                entry.metatraffic_unicast_locators = participant.metatraffic_unicast_locators;
                entry.metatraffic_multicast_locators = participant.metatraffic_multicast_locators;
                entry.last_seen_at = observed_at;
                entry.expires_at = expires_at;
                self.refresh_endpoint_expiration_for_participant(gid, expires_at, observed_at);
                DiscoveryChange::Updated
            }
            None => {
                self.participants.insert(
                    gid,
                    ParticipantEntry {
                        gid,
                        lease_duration: participant.lease_duration,
                        default_unicast_locators: participant.default_unicast_locators,
                        metatraffic_unicast_locators: participant.metatraffic_unicast_locators,
                        metatraffic_multicast_locators: participant
                            .metatraffic_multicast_locators,
                        first_seen_at: observed_at,
                        last_seen_at: observed_at,
                        expires_at,
                    },
                );
                self.refresh_endpoint_expiration_for_participant(gid, expires_at, observed_at);
                DiscoveryChange::Inserted
            }
        }
    }

    fn refresh_endpoint_expiration_for_participant(
        &mut self,
        participant_gid: TopicGid,
        participant_expires_at: Option<SystemTime>,
        observed_at: SystemTime,
    ) {
        let endpoint_deadline = observed_at.checked_add(self.default_endpoint_ttl);
        let refreshed_expires_at = match (participant_expires_at, endpoint_deadline) {
            (Some(participant_deadline), Some(endpoint_deadline)) => {
                Some(std::cmp::max(participant_deadline, endpoint_deadline))
            }
            (Some(participant_deadline), None) => Some(participant_deadline),
            (None, Some(endpoint_deadline)) => Some(endpoint_deadline),
            (None, None) => None,
        };

        refresh_endpoint_map_expiration(
            &mut self.publications,
            participant_gid,
            refreshed_expires_at,
        );
        refresh_endpoint_map_expiration(
            &mut self.subscriptions,
            participant_gid,
            refreshed_expires_at,
        );
    }

    fn endpoint_expires_at(
        &self,
        participant_gid: Option<TopicGid>,
        observed_at: SystemTime,
    ) -> Option<SystemTime> {
        let endpoint_deadline = observed_at.checked_add(self.default_endpoint_ttl);
        let participant_deadline =
            participant_gid.and_then(|gid| self.participants.get(&gid).and_then(|entry| entry.expires_at));

        match (participant_deadline, endpoint_deadline) {
            (Some(participant_deadline), Some(endpoint_deadline)) => {
                Some(std::cmp::max(participant_deadline, endpoint_deadline))
            }
            (Some(participant_deadline), None) => Some(participant_deadline),
            (None, Some(endpoint_deadline)) => Some(endpoint_deadline),
            (None, None) => None,
        }
    }

    fn upsert_endpoint(
        map: &mut BTreeMap<TopicGid, EndpointEntry>,
        endpoint: DiscoveredEndpoint,
        observed_at: SystemTime,
        expires_at: Option<SystemTime>,
    ) -> DiscoveryChange {
        let Some(endpoint_gid) = endpoint.endpoint_gid else {
            return DiscoveryChange::Noop;
        };

        match map.get_mut(&endpoint_gid) {
            Some(entry) => {
                // NOTE: we intentionally always return Updated here even when the
                // endpoint fields are unchanged. CommandState.topic_details depends
                // on the external `remote_participants` set which can change between
                // equal SEDP beats; skipping refresh on unchanged SEDP would leave
                // topic.local_only stale when a participant's IP migrates.
                entry.participant_gid = endpoint.participant_gid;
                entry.topic_name = endpoint.topic_name;
                entry.type_name = endpoint.type_name;
                entry.reliability = endpoint.reliability;
                entry.durability = endpoint.durability;
                entry.deadline = endpoint.deadline;
                entry.latency_budget = endpoint.latency_budget;
                entry.lifespan = endpoint.lifespan;
                entry.liveliness = endpoint.liveliness;
                entry.unicast_locators = endpoint.unicast_locators;
                entry.multicast_locators = endpoint.multicast_locators;
                entry.last_seen_at = observed_at;
                entry.expires_at = expires_at;
                DiscoveryChange::Updated
            }
            None => {
                map.insert(
                    endpoint_gid,
                    EndpointEntry {
                        endpoint_gid,
                        participant_gid: endpoint.participant_gid,
                        topic_name: endpoint.topic_name,
                        type_name: endpoint.type_name,
                        reliability: endpoint.reliability,
                        durability: endpoint.durability,
                        deadline: endpoint.deadline,
                        latency_budget: endpoint.latency_budget,
                        lifespan: endpoint.lifespan,
                        liveliness: endpoint.liveliness,
                        unicast_locators: endpoint.unicast_locators,
                        multicast_locators: endpoint.multicast_locators,
                        first_seen_at: observed_at,
                        last_seen_at: observed_at,
                        expires_at,
                    },
                );
                DiscoveryChange::Inserted
            }
        }
    }
}

impl NodeTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn nodes(&self) -> &BTreeMap<NodeKey, NodeEntry> {
        &self.nodes
    }

    pub fn node(&self, key: &NodeKey) -> Option<&NodeEntry> {
        self.nodes.get(key)
    }

    pub fn node_for_writer(&self, gid: &TopicGid) -> Option<&NodeEntry> {
        self.writer_index.get(gid).and_then(|key| self.nodes.get(key))
    }

    pub fn node_for_reader(&self, gid: &TopicGid) -> Option<&NodeEntry> {
        self.reader_index.get(gid).and_then(|key| self.nodes.get(key))
    }

    pub fn nodes_for_participant(&self, participant_gid: &TopicGid) -> Vec<&NodeEntry> {
        self.participant_index
            .get(participant_gid)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .filter_map(|key| self.nodes.get(key))
            .collect()
    }

    pub fn upsert_sample(&mut self, sample: NodeSample, observed_at: SystemTime) {
        let key = NodeKey {
            participant_gid: sample.participant_gid,
            node_namespace: sample.node_namespace,
            node_name: sample.node_name,
        };

        if let Some(previous) = self.nodes.get(&key).cloned() {
            self.remove_indexes(&previous);
        }

        let mut writer_gids = BTreeSet::new();
        writer_gids.extend(sample.writer_gids);
        let mut reader_gids = BTreeSet::new();
        reader_gids.extend(sample.reader_gids);

        let first_seen_at = self
            .nodes
            .get(&key)
            .map(|entry| entry.first_seen_at)
            .unwrap_or(observed_at);
        let entry = NodeEntry {
            key: key.clone(),
            writer_gids,
            reader_gids,
            first_seen_at,
            last_seen_at: observed_at,
        };

        self.nodes.insert(key.clone(), entry.clone());
        self.insert_indexes(&entry);
    }

    pub fn replace_participant_nodes(
        &mut self,
        participant_gid: TopicGid,
        samples: Vec<NodeSample>,
        observed_at: SystemTime,
    ) {
        let retained = samples
            .iter()
            .map(|sample| NodeKey {
                participant_gid: sample.participant_gid,
                node_namespace: sample.node_namespace.clone(),
                node_name: sample.node_name.clone(),
            })
            .collect::<BTreeSet<_>>();

        let stale = self
            .participant_index
            .get(&participant_gid)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .filter(|key| !retained.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in stale {
            let _ = self.remove_node(&key);
        }

        for sample in samples {
            self.upsert_sample(sample, observed_at);
        }
    }

    pub fn remove_node(&mut self, key: &NodeKey) -> Option<NodeEntry> {
        let removed = self.nodes.remove(key)?;
        self.remove_indexes(&removed);
        Some(removed)
    }

    fn insert_indexes(&mut self, entry: &NodeEntry) {
        self.participant_index
            .entry(entry.key.participant_gid)
            .or_default()
            .insert(entry.key.clone());

        for gid in &entry.writer_gids {
            self.writer_index.insert(*gid, entry.key.clone());
        }
        for gid in &entry.reader_gids {
            self.reader_index.insert(*gid, entry.key.clone());
        }
    }

    fn remove_indexes(&mut self, entry: &NodeEntry) {
        for gid in &entry.writer_gids {
            self.writer_index.remove(gid);
        }
        for gid in &entry.reader_gids {
            self.reader_index.remove(gid);
        }

        if let Some(keys) = self.participant_index.get_mut(&entry.key.participant_gid) {
            keys.remove(&entry.key);
            if keys.is_empty() {
                self.participant_index.remove(&entry.key.participant_gid);
            }
        }
    }
}

impl Default for DiscoveryTable {
    fn default() -> Self {
        Self::new(Duration::from_secs(20))
    }
}

#[derive(Default)]
struct TopicAccumulator {
    type_names: BTreeSet<String>,
    publisher_gids: Vec<TopicGid>,
    subscription_gids: Vec<TopicGid>,
}

fn expire_endpoints(
    entries: &mut BTreeMap<TopicGid, EndpointEntry>,
    now: SystemTime,
) -> usize {
    let expired = entries
        .iter()
        .filter_map(|(gid, entry)| is_expired(entry.expires_at, now).then_some(*gid))
        .collect::<Vec<_>>();

    let removed = expired.len();
    for gid in expired {
        entries.remove(&gid);
    }
    removed
}

fn refresh_endpoint_map_expiration(
    entries: &mut BTreeMap<TopicGid, EndpointEntry>,
    participant_gid: TopicGid,
    expires_at: Option<SystemTime>,
) {
    for entry in entries.values_mut() {
        if entry.participant_gid == Some(participant_gid) {
            entry.expires_at = expires_at;
        }
    }
}

fn is_expired(expires_at: Option<SystemTime>, now: SystemTime) -> bool {
    matches!(expires_at, Some(deadline) if deadline <= now)
}

fn duration_value_to_duration(value: DurationValue) -> Option<Duration> {
    if value.seconds < 0 {
        return None;
    }
    Some(Duration::new(value.seconds as u64, value.fraction))
}
