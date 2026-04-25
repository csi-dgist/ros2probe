use anyhow::{Context, bail};
use ros2probe_common::TopicGid;

use ros2probe_common::RTPS_PARTICIPANT_ENTITY_ID;

use crate::protocols::rtps::{
    DiscoveryKind as NewDiscoveryKind, RtpsEvent, RtpsMessage as NewRtpsMessage,
    RtpsMessageKind as NewRtpsMessageKind,
};

const PID_SENTINEL: u16 = 0x0001;
const PID_PARTICIPANT_LEASE_DURATION: u16 = 0x0002;
const PID_TOPIC_NAME: u16 = 0x0005;
const PID_TYPE_NAME: u16 = 0x0007;
const PID_RELIABILITY: u16 = 0x001a;
const PID_DURABILITY: u16 = 0x001d;
const PID_DEADLINE: u16 = 0x0023;
const PID_LATENCY_BUDGET: u16 = 0x0027;
const PID_LIFESPAN: u16 = 0x002b;
const PID_UNICAST_LOCATOR: u16 = 0x002f;
const PID_MULTICAST_LOCATOR: u16 = 0x0030;
const PID_DEFAULT_UNICAST_LOCATOR: u16 = 0x0031;
const PID_METATRAFFIC_UNICAST_LOCATOR: u16 = 0x0032;
const PID_METATRAFFIC_MULTICAST_LOCATOR: u16 = 0x0033;
const PID_LIVELINESS: u16 = 0x001b;
const PID_PARTICIPANT_GUID: u16 = 0x0050;
const PID_ENDPOINT_GUID: u16 = 0x005a;

#[derive(Clone, Debug)]
pub enum DiscoverySample {
    Participant(DiscoveredParticipant),
    Publication(DiscoveredEndpoint),
    Subscription(DiscoveredEndpoint),
    UnknownBuiltin(DiscoveredUnknown),
    /// A participant or endpoint announced its own removal via PID_STATUS_INFO disposal.
    Disposed { kind: DiscoveryBuiltinKind, gid: TopicGid },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscoveredParticipant {
    pub guid: Option<TopicGid>,
    pub lease_duration: Option<DurationValue>,
    pub default_unicast_locators: Vec<Locator>,
    pub metatraffic_unicast_locators: Vec<Locator>,
    pub metatraffic_multicast_locators: Vec<Locator>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiscoveredEndpoint {
    pub endpoint_gid: Option<TopicGid>,
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
}

#[derive(Clone, Debug)]
pub struct DiscoveredUnknown {
    pub kind: DiscoveryBuiltinKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryBuiltinKind {
    Participant,
    Publication,
    Subscription,
    ParticipantMessage,
    UnknownBuiltin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurationValue {
    pub seconds: i32,
    pub fraction: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Locator {
    pub kind: i32,
    pub port: u32,
    pub address: [u8; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReliabilityQos {
    pub kind: u32,
    pub max_blocking_time: DurationValue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityQos {
    pub kind: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LivelinessQos {
    pub kind: u32,
    pub lease_duration: DurationValue,
}

pub fn parse_event(event: &RtpsEvent) -> anyhow::Result<Option<DiscoverySample>> {
    match event {
        RtpsEvent::Discovery(message) => parse_message(message),
        RtpsEvent::Message(_) => Ok(None),
    }
}

pub fn parse_message<M>(message: &M) -> anyhow::Result<Option<DiscoverySample>>
where
    M: DiscoveryMessageView,
{
    let Some(kind) = message.discovery_kind() else {
        return Ok(None);
    };

    // Handle graceful disposal: PID_STATUS_INFO with DISPOSED|UNREGISTERED bits.
    if message.is_disposed() {
        let gid = match kind {
            DiscoveryBuiltinKind::Participant => {
                // Participant GUID = same guid_prefix + well-known participant entity ID.
                TopicGid::from_rtps_parts(
                    message.writer_gid().guid_prefix(),
                    RTPS_PARTICIPANT_ENTITY_ID,
                )
            }
            DiscoveryBuiltinKind::Publication | DiscoveryBuiltinKind::Subscription => {
                // Key payload (after 4-byte CDR encapsulation header) = 16-byte endpoint GUID.
                let payload = message.payload();
                if payload.len() >= 20 {
                    let mut bytes = [0u8; 16];
                    bytes.copy_from_slice(&payload[4..20]);
                    TopicGid::new(bytes)
                } else {
                    // Payload absent or too short; endpoint will expire via TTL.
                    return Ok(None);
                }
            }
            _ => return Ok(None),
        };
        return Ok(Some(DiscoverySample::Disposed { kind, gid }));
    }

    let payload = message.payload();
    let little_endian = decode_encapsulation(payload)?;

    Ok(Some(match kind {
        DiscoveryBuiltinKind::Participant => {
            DiscoverySample::Participant(parse_participant_body(payload, little_endian)?)
        }
        DiscoveryBuiltinKind::Publication => {
            DiscoverySample::Publication(parse_endpoint_body(payload, little_endian)?)
        }
        DiscoveryBuiltinKind::Subscription => {
            DiscoverySample::Subscription(parse_endpoint_body(payload, little_endian)?)
        }
        DiscoveryBuiltinKind::ParticipantMessage | DiscoveryBuiltinKind::UnknownBuiltin => {
            DiscoverySample::UnknownBuiltin(DiscoveredUnknown { kind })
        }
    }))
}

pub trait DiscoveryMessageView {
    fn payload(&self) -> &[u8];
    fn frame_len(&self) -> usize;
    fn direction(&self) -> crate::capture::PacketDirection;
    fn src_ip(&self) -> ros2probe_common::IpAddr;
    fn dst_ip(&self) -> ros2probe_common::IpAddr;
    fn udp_payload_len(&self) -> usize;
    fn discovery_kind(&self) -> Option<DiscoveryBuiltinKind>;
    fn writer_gid(&self) -> TopicGid;
    fn is_disposed(&self) -> bool;
}

impl DiscoveryMessageView for NewRtpsMessage {
    fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn frame_len(&self) -> usize {
        self.frame_len
    }

    fn direction(&self) -> crate::capture::PacketDirection {
        self.direction
    }

    fn src_ip(&self) -> ros2probe_common::IpAddr {
        self.src_ip
    }

    fn dst_ip(&self) -> ros2probe_common::IpAddr {
        self.dst_ip
    }

    fn udp_payload_len(&self) -> usize {
        self.udp_payload_len
    }

    fn discovery_kind(&self) -> Option<DiscoveryBuiltinKind> {
        match self.kind {
            NewRtpsMessageKind::Discovery(NewDiscoveryKind::Participant) => {
                Some(DiscoveryBuiltinKind::Participant)
            }
            NewRtpsMessageKind::Discovery(NewDiscoveryKind::Publication) => {
                Some(DiscoveryBuiltinKind::Publication)
            }
            NewRtpsMessageKind::Discovery(NewDiscoveryKind::Subscription) => {
                Some(DiscoveryBuiltinKind::Subscription)
            }
            NewRtpsMessageKind::Discovery(NewDiscoveryKind::UnknownBuiltin) => {
                Some(DiscoveryBuiltinKind::UnknownBuiltin)
            }
            NewRtpsMessageKind::UserData => None,
        }
    }

    fn writer_gid(&self) -> TopicGid {
        self.writer_gid
    }

    fn is_disposed(&self) -> bool {
        self.disposed
    }
}

fn decode_encapsulation(payload: &[u8]) -> anyhow::Result<bool> {
    if payload.len() < 4 {
        bail!("discovery payload shorter than encapsulation header");
    }
    let encapsulation_kind = u16::from_be_bytes([payload[0], payload[1]]);
    match encapsulation_kind {
        0x0000 | 0x0002 => Ok(false),
        0x0001 | 0x0003 => Ok(true),
        kind => bail!("unsupported discovery encapsulation kind {kind:#06x}"),
    }
}

/// Walk the PL_CDR parameter list in one pass. For each parameter, call `f(id, value)`.
fn iter_parameters(
    payload: &[u8],
    little_endian: bool,
    mut f: impl FnMut(u16, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut offset = 4usize;
    while offset + 4 <= payload.len() {
        let id = read_u16(&payload[offset..offset + 2], little_endian);
        let value_len = usize::from(read_u16(&payload[offset + 2..offset + 4], little_endian));
        offset += 4;

        if id == PID_SENTINEL {
            return Ok(());
        }

        let value_bytes = payload
            .get(offset..offset + value_len)
            .with_context(|| format!("parameter {id:#06x} value out of bounds"))?;
        f(id, value_bytes)?;

        // RTPS §9.4.2.11: each parameter starts on a 4-byte boundary.
        let padded_len = (value_len + 3) & !3;
        offset = offset
            .checked_add(padded_len)
            .context("discovery parameter length overflow")?;
    }
    Ok(())
}

fn parse_participant_body(
    payload: &[u8],
    little_endian: bool,
) -> anyhow::Result<DiscoveredParticipant> {
    let mut out = DiscoveredParticipant::default();
    iter_parameters(payload, little_endian, |id, value| {
        match id {
            PID_PARTICIPANT_GUID => {
                out.guid = Some(parse_guid(value)?);
            }
            PID_PARTICIPANT_LEASE_DURATION => {
                out.lease_duration = Some(parse_duration(value, little_endian)?);
            }
            PID_DEFAULT_UNICAST_LOCATOR => {
                out.default_unicast_locators
                    .push(parse_locator(value, little_endian)?);
            }
            PID_METATRAFFIC_UNICAST_LOCATOR => {
                out.metatraffic_unicast_locators
                    .push(parse_locator(value, little_endian)?);
            }
            PID_METATRAFFIC_MULTICAST_LOCATOR => {
                out.metatraffic_multicast_locators
                    .push(parse_locator(value, little_endian)?);
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}

fn parse_endpoint_body(
    payload: &[u8],
    little_endian: bool,
) -> anyhow::Result<DiscoveredEndpoint> {
    let mut out = DiscoveredEndpoint::default();
    iter_parameters(payload, little_endian, |id, value| {
        match id {
            PID_ENDPOINT_GUID => {
                out.endpoint_gid = Some(parse_guid(value)?);
            }
            PID_PARTICIPANT_GUID => {
                out.participant_gid = Some(parse_guid(value)?);
            }
            PID_TOPIC_NAME => {
                out.topic_name = Some(parse_cdr_string(value, little_endian)?);
            }
            PID_TYPE_NAME => {
                out.type_name = Some(parse_cdr_string(value, little_endian)?);
            }
            PID_RELIABILITY => {
                out.reliability = Some(parse_reliability(value, little_endian)?);
            }
            PID_DURABILITY => {
                out.durability = Some(parse_durability(value, little_endian)?);
            }
            PID_DEADLINE => {
                out.deadline = Some(parse_duration(value, little_endian)?);
            }
            PID_LATENCY_BUDGET => {
                out.latency_budget = Some(parse_duration(value, little_endian)?);
            }
            PID_LIFESPAN => {
                out.lifespan = Some(parse_duration(value, little_endian)?);
            }
            PID_LIVELINESS => {
                out.liveliness = Some(parse_liveliness(value, little_endian)?);
            }
            PID_UNICAST_LOCATOR => {
                out.unicast_locators
                    .push(parse_locator(value, little_endian)?);
            }
            PID_MULTICAST_LOCATOR => {
                out.multicast_locators
                    .push(parse_locator(value, little_endian)?);
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(out)
}

fn parse_guid(value: &[u8]) -> anyhow::Result<TopicGid> {
    let bytes = value
        .get(..16)
        .context("GUID parameter shorter than 16 bytes")?;
    let mut gid = [0u8; 16];
    gid.copy_from_slice(bytes);
    Ok(TopicGid::new(gid))
}

fn parse_cdr_string(value: &[u8], little_endian: bool) -> anyhow::Result<String> {
    let len_bytes = value
        .get(..4)
        .context("CDR string shorter than length prefix")?;
    let declared_len = usize::try_from(read_u32(len_bytes, little_endian))
        .context("CDR string length does not fit usize")?;
    if declared_len == 0 {
        return Ok(String::new());
    }

    let raw = value
        .get(4..4 + declared_len)
        .context("CDR string body out of bounds")?;
    let without_nul = raw.strip_suffix(&[0]).unwrap_or(raw);
    std::str::from_utf8(without_nul)
        .map(str::to_owned)
        .context("decode discovery string")
}

fn parse_duration(value: &[u8], little_endian: bool) -> anyhow::Result<DurationValue> {
    if value.len() < 8 {
        bail!("duration parameter shorter than 8 bytes");
    }
    Ok(DurationValue {
        seconds: read_i32(&value[..4], little_endian),
        fraction: read_u32(&value[4..8], little_endian),
    })
}

fn parse_locator(value: &[u8], little_endian: bool) -> anyhow::Result<Locator> {
    if value.len() < 24 {
        bail!("locator parameter shorter than 24 bytes");
    }
    let mut address = [0u8; 16];
    address.copy_from_slice(&value[8..24]);
    Ok(Locator {
        kind: read_i32(&value[..4], little_endian),
        port: read_u32(&value[4..8], little_endian),
        address,
    })
}

fn parse_reliability(value: &[u8], little_endian: bool) -> anyhow::Result<ReliabilityQos> {
    if value.len() < 12 {
        bail!("reliability parameter shorter than 12 bytes");
    }
    Ok(ReliabilityQos {
        kind: read_u32(&value[..4], little_endian),
        max_blocking_time: DurationValue {
            seconds: read_i32(&value[4..8], little_endian),
            fraction: read_u32(&value[8..12], little_endian),
        },
    })
}

fn parse_durability(value: &[u8], little_endian: bool) -> anyhow::Result<DurabilityQos> {
    if value.len() < 4 {
        bail!("durability parameter shorter than 4 bytes");
    }
    Ok(DurabilityQos {
        kind: read_u32(&value[..4], little_endian),
    })
}

fn parse_liveliness(value: &[u8], little_endian: bool) -> anyhow::Result<LivelinessQos> {
    if value.len() < 12 {
        bail!("liveliness parameter shorter than 12 bytes");
    }
    Ok(LivelinessQos {
        kind: read_u32(&value[..4], little_endian),
        lease_duration: DurationValue {
            seconds: read_i32(&value[4..8], little_endian),
            fraction: read_u32(&value[8..12], little_endian),
        },
    })
}

fn read_u16(bytes: &[u8], little_endian: bool) -> u16 {
    if little_endian {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_be_bytes([bytes[0], bytes[1]])
    }
}

fn read_u32(bytes: &[u8], little_endian: bool) -> u32 {
    if little_endian {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

fn read_i32(bytes: &[u8], little_endian: bool) -> i32 {
    if little_endian {
        i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}
