use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use aya::{
    Ebpf,
    maps::{HashMap, MapData},
};
use ros2probe_common::{MAX_TOPIC_GIDS, TOPIC_GID_STATE_AVAILABLE, TopicGid};

use crate::discovery::{DiscoveryTable, EndpointEntry};

const TOPIC_FILTER_GIDS_MAP: &str = "TOPIC_FILTER_GIDS";
const ROS_DISCOVERY_INFO_TOPIC: &str = "/ros_discovery_info";

#[derive(Clone, Debug)]
pub struct RecorderTopicMetadata {
    pub topic_name: String,
    pub type_name: Option<String>,
    pub participant_gid: Option<TopicGid>,
}

#[derive(Clone, Debug)]
pub enum GidMapMode {
    /// Pass every user topic's DATA. Used by `rp bag record --all`.
    All,
    /// Drop every user topic's DATA. Used when no observer or recorder is
    /// active — userspace doesn't need the traffic, so the eBPF filter
    /// rejects it in-kernel. The `ROS_DISCOVERY_INFO_TOPIC` is still matched
    /// regardless so `node_table` can be maintained.
    None,
    Explicit(BTreeSet<String>),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GidMapSyncStats {
    pub inserted: usize,
    pub updated: usize,
    pub removed: usize,
    pub retained: usize,
}

pub struct RecorderTopicGidMap {
    gids_map: HashMap<MapData, TopicGid, u8>,
    mode: GidMapMode,
    current: BTreeMap<TopicGid, RecorderTopicMetadata>,
}

impl RecorderTopicGidMap {
    pub fn from_ebpf(ebpf: &mut Ebpf) -> anyhow::Result<Self> {
        let gids_map = HashMap::<_, TopicGid, u8>::try_from(
            ebpf.take_map(TOPIC_FILTER_GIDS_MAP)
                .context("take TOPIC_FILTER_GIDS map")?,
        )
        .context("open TOPIC_FILTER_GIDS map")?;

        Ok(Self {
            gids_map,
            // Start in None: the filter drops all user DATA until an observer
            // or recorder is started. Discovery traffic passes regardless via
            // eBPF's `is_discovery` bypass.
            mode: GidMapMode::None,
            current: BTreeMap::new(),
        })
    }

    pub fn configure(&mut self, topics: &[String]) {
        self.mode = build_mode(topics);
    }

    /// Set the filter to reject every user topic's DATA. Discovery and
    /// `/ros_discovery_info` still flow via their dedicated pass paths.
    pub fn configure_none(&mut self) {
        self.mode = GidMapMode::None;
    }

    pub fn mode(&self) -> &GidMapMode {
        &self.mode
    }

    pub fn current(&self) -> &BTreeMap<TopicGid, RecorderTopicMetadata> {
        &self.current
    }

    pub fn metadata_for_gid(&self, gid: &TopicGid) -> Option<&RecorderTopicMetadata> {
        self.current.get(gid)
    }

    pub fn metadata_for_message(
        &self,
        writer_gid: &TopicGid,
        reader_gid: &TopicGid,
    ) -> Option<&RecorderTopicMetadata> {
        self.metadata_for_gid(writer_gid)
            .or_else(|| self.metadata_for_gid(reader_gid))
    }

    pub fn contains_gid(&self, gid: &TopicGid) -> bool {
        self.current.contains_key(gid)
    }

    pub fn rebuild_from_table(&mut self, table: &DiscoveryTable) -> anyhow::Result<GidMapSyncStats> {
        let next = self.select_from_table(table);
        if next.len() > MAX_TOPIC_GIDS as usize {
            bail!(
                "recorder gid map produced {} gids, exceeding eBPF map capacity {}",
                next.len(),
                MAX_TOPIC_GIDS
            );
        }
        self.sync(next)
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let current = self.current.keys().copied().collect::<Vec<_>>();
        for gid in current {
            self.gids_map
                .remove(&gid)
                .with_context(|| format!("remove recorder gid {}", format_gid(&gid)))?;
        }
        self.current.clear();
        Ok(())
    }

    fn select_from_table(&self, table: &DiscoveryTable) -> BTreeMap<TopicGid, RecorderTopicMetadata> {
        let mut selected = table
            .publications()
            .iter()
            .filter_map(|(gid, endpoint)| {
                let topic_name = endpoint.topic_name.as_ref()?;
                let normalized_topic_name = normalize_topic_name(topic_name);
                if !self.matches_topic(&normalized_topic_name) {
                    return None;
                }
                Some((*gid, build_metadata(endpoint, &normalized_topic_name)))
            })
            .collect::<BTreeMap<_, _>>();

        for (gid, endpoint) in table.subscriptions() {
            let Some(topic_name) = endpoint.topic_name.as_ref() else {
                continue;
            };
            let normalized_topic_name = normalize_topic_name(topic_name);
            if normalized_topic_name != ROS_DISCOVERY_INFO_TOPIC {
                continue;
            }
            selected.insert(*gid, build_metadata(endpoint, &normalized_topic_name));
        }

        selected
    }

    fn matches_topic(&self, topic_name: &str) -> bool {
        if topic_name == ROS_DISCOVERY_INFO_TOPIC {
            return true;
        }

        match &self.mode {
            GidMapMode::All => true,
            GidMapMode::None => false,
            GidMapMode::Explicit(topics) => topics.contains(topic_name),
        }
    }

    fn sync(&mut self, next: BTreeMap<TopicGid, RecorderTopicMetadata>) -> anyhow::Result<GidMapSyncStats> {
        let mut stats = GidMapSyncStats::default();

        let removed = self
            .current
            .keys()
            .filter(|gid| !next.contains_key(*gid))
            .copied()
            .collect::<Vec<_>>();
        for gid in removed {
            self.gids_map
                .remove(&gid)
                .with_context(|| format!("remove recorder gid {}", format_gid(&gid)))?;
            self.current.remove(&gid);
            stats.removed += 1;
        }

        for (gid, metadata) in next {
            match self.current.get(&gid) {
                Some(existing) if same_metadata(existing, &metadata) => {
                    stats.retained += 1;
                }
                Some(_) => {
                    self.gids_map
                        .insert(gid, TOPIC_GID_STATE_AVAILABLE, 0)
                        .with_context(|| format!("update recorder gid {}", format_gid(&gid)))?;
                    self.current.insert(gid, metadata);
                    stats.updated += 1;
                }
                None => {
                    self.gids_map
                        .insert(gid, TOPIC_GID_STATE_AVAILABLE, 0)
                        .with_context(|| format!("insert recorder gid {}", format_gid(&gid)))?;
                    self.current.insert(gid, metadata);
                    stats.inserted += 1;
                }
            }
        }

        Ok(stats)
    }
}

fn build_mode(topics: &[String]) -> GidMapMode {
    if topics.is_empty() || topics.iter().any(|topic| topic.eq_ignore_ascii_case("all")) {
        GidMapMode::All
    } else {
        GidMapMode::Explicit(topics.iter().cloned().collect())
    }
}

fn build_metadata(endpoint: &EndpointEntry, topic_name: &str) -> RecorderTopicMetadata {
    RecorderTopicMetadata {
        topic_name: normalize_topic_name(topic_name),
        type_name: endpoint.type_name.as_deref().map(normalize_type_name),
        participant_gid: endpoint.participant_gid,
    }
}

fn same_metadata(left: &RecorderTopicMetadata, right: &RecorderTopicMetadata) -> bool {
    left.topic_name == right.topic_name
        && left.type_name == right.type_name
        && left.participant_gid == right.participant_gid
}

fn format_gid(gid: &TopicGid) -> String {
    gid.bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn normalize_topic_name(topic_name: &str) -> String {
    if let Some(stripped) = topic_name.strip_prefix("rt/") {
        format!("/{stripped}")
    } else if topic_name.starts_with('/') {
        topic_name.to_string()
    } else {
        format!("/{topic_name}")
    }
}

fn normalize_type_name(type_name: &str) -> String {
    if type_name.contains('/') {
        return type_name.to_string();
    }

    let Some((package, remainder)) = type_name.split_once("::") else {
        return type_name.to_string();
    };

    let (kind, remainder) = if let Some(rest) = remainder.strip_prefix("msg::dds_::") {
        ("msg", rest)
    } else if let Some(rest) = remainder.strip_prefix("srv::dds_::") {
        ("srv", rest)
    } else if let Some(rest) = remainder.strip_prefix("action::dds_::") {
        ("action", rest)
    } else {
        return type_name.to_string();
    };

    let type_name = remainder.strip_suffix('_').unwrap_or(remainder);
    format!("{package}/{kind}/{type_name}")
}
