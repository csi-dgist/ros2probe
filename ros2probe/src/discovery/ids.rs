use ros2probe_common::TopicGid;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Middleware {
    Rtps,
    Zenoh,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ParticipantId {
    Rtps(TopicGid),
    Zenoh {
        domain_id: u64,
        zid: String,
        node_id: String,
    },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EndpointId {
    Rtps(TopicGid),
    Zenoh {
        domain_id: u64,
        zid: String,
        node_id: String,
        entity_id: String,
        kind: String,
    },
}

impl ParticipantId {
    pub fn rtps(gid: TopicGid) -> Self {
        Self::Rtps(gid)
    }

    pub fn middleware(&self) -> Middleware {
        match self {
            Self::Rtps(_) => Middleware::Rtps,
            Self::Zenoh { .. } => Middleware::Zenoh,
        }
    }

    pub fn rtps_gid(&self) -> Option<TopicGid> {
        match self {
            Self::Rtps(gid) => Some(*gid),
            Self::Zenoh { .. } => None,
        }
    }
}

impl EndpointId {
    pub fn rtps(gid: TopicGid) -> Self {
        Self::Rtps(gid)
    }

    pub fn middleware(&self) -> Middleware {
        match self {
            Self::Rtps(_) => Middleware::Rtps,
            Self::Zenoh { .. } => Middleware::Zenoh,
        }
    }

    pub fn rtps_gid(&self) -> Option<TopicGid> {
        match self {
            Self::Rtps(gid) => Some(*gid),
            Self::Zenoh { .. } => None,
        }
    }
}
