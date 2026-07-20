use anyhow::{Context, bail};
use ros2probe_common::TopicGid;

use crate::discovery::NodeSample;

const CDR_BE: u16 = 0x0000;
const CDR_LE: u16 = 0x0001;
const ROS_DISCOVERY_INFO_TOPIC: &str = "/ros_discovery_info";
const ROS_DISCOVERY_INFO_TYPE: &str = "rmw_dds_common/msg/ParticipantEntitiesInfo";
const ROS_GID_LEN: usize = 16;
const ROS_GID_LEN_LEGACY: usize = 24;
const NODE_ENTITY_MIN_SERIALIZED_LEN: usize = 16;
const MAX_ROS_DISCOVERY_NODES: usize = 4096;
const MAX_ROS_DISCOVERY_GIDS_PER_NODE: usize = 16384;
const MAX_ROS_DISCOVERY_STRING_LEN: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ParticipantEntitiesInfo {
    pub participant_gid: TopicGid,
    pub nodes: Vec<NodeSample>,
}

pub fn is_ros_discovery_info(topic_name: &str) -> bool {
    topic_name == ROS_DISCOVERY_INFO_TOPIC || topic_name == "rt/ros_discovery_info"
}

pub fn ros_discovery_info_type() -> &'static str {
    ROS_DISCOVERY_INFO_TYPE
}

pub fn parse_participant_entities_info(payload: &[u8]) -> anyhow::Result<ParticipantEntitiesInfo> {
    if payload.len() < 4 {
        bail!("ros_discovery_info payload shorter than CDR encapsulation header");
    }

    let encapsulation = u16::from_be_bytes([payload[0], payload[1]]);
    let little_endian = match encapsulation {
        CDR_BE => false,
        CDR_LE => true,
        kind => bail!("unsupported ros_discovery_info encapsulation kind {kind:#06x}"),
    };

    parse_participant_entities_info_body(&payload[4..], little_endian, ROS_GID_LEN).or_else(|err| {
        parse_participant_entities_info_body(&payload[4..], little_endian, ROS_GID_LEN_LEGACY)
            .with_context(|| {
                format!("parse ros_discovery_info with {ROS_GID_LEN}-byte GID failed: {err:#}")
            })
    })
}

fn parse_participant_entities_info_body(
    payload: &[u8],
    little_endian: bool,
    gid_len: usize,
) -> anyhow::Result<ParticipantEntitiesInfo> {
    let mut reader = CdrReader::new(payload, little_endian, gid_len);
    let participant_gid = reader.read_topic_gid()?;
    let node_count = reader.read_bounded_len(
        "node entity sequence",
        MAX_ROS_DISCOVERY_NODES,
        NODE_ENTITY_MIN_SERIALIZED_LEN,
    )?;
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let node_namespace = reader.read_string()?;
        let node_name = reader.read_string()?;
        let reader_gids = reader.read_gid_sequence()?;
        let writer_gids = reader.read_gid_sequence()?;
        nodes.push(NodeSample {
            participant_gid: Some(participant_gid),
            participant_id: None,
            node_namespace,
            node_name,
            writer_gids,
            reader_gids,
            writer_endpoint_ids: Vec::new(),
            reader_endpoint_ids: Vec::new(),
        });
    }
    reader.finish()?;

    Ok(ParticipantEntitiesInfo {
        participant_gid,
        nodes,
    })
}

struct CdrReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    little_endian: bool,
    gid_len: usize,
}

impl<'a> CdrReader<'a> {
    fn new(bytes: &'a [u8], little_endian: bool, gid_len: usize) -> Self {
        Self {
            bytes,
            offset: 0,
            little_endian,
            gid_len,
        }
    }

    fn read_u32(&mut self) -> anyhow::Result<u32> {
        self.align(4)?;
        let end = self.offset.checked_add(4).context("u32 offset overflow")?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .context("u32 out of bounds")?;
        self.offset = end;
        Ok(if self.little_endian {
            u32::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u32::from_be_bytes(bytes.try_into().unwrap())
        })
    }

    fn read_string(&mut self) -> anyhow::Result<String> {
        self.align(4)?;
        let len = self.read_u32()? as usize;
        if len > MAX_ROS_DISCOVERY_STRING_LEN {
            bail!("CDR string length {len} exceeds maximum {MAX_ROS_DISCOVERY_STRING_LEN}");
        }
        if len == 0 {
            return Ok(String::new());
        }

        let end = self
            .offset
            .checked_add(len)
            .context("string offset overflow")?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .context("string bytes out of bounds")?;
        self.offset = end;
        let string_bytes = bytes
            .strip_suffix(&[0])
            .context("CDR string missing trailing null terminator")?;
        Ok(std::str::from_utf8(string_bytes)
            .context("decode UTF-8 string")?
            .to_string())
    }

    fn read_gid_sequence(&mut self) -> anyhow::Result<Vec<TopicGid>> {
        let len = self.read_bounded_len(
            "GID sequence",
            MAX_ROS_DISCOVERY_GIDS_PER_NODE,
            self.gid_len,
        )?;
        let mut gids = Vec::with_capacity(len);
        for _ in 0..len {
            gids.push(self.read_topic_gid()?);
        }
        Ok(gids)
    }

    fn read_topic_gid(&mut self) -> anyhow::Result<TopicGid> {
        let end = self
            .offset
            .checked_add(self.gid_len)
            .context("GID offset overflow")?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .context("GID bytes out of bounds")?;
        self.offset = end;

        let mut gid = [0u8; 16];
        gid.copy_from_slice(&bytes[..16]);
        Ok(TopicGid::new(gid))
    }

    fn read_bounded_len(
        &mut self,
        what: &str,
        hard_limit: usize,
        min_serialized_item_len: usize,
    ) -> anyhow::Result<usize> {
        let len = self.read_u32()? as usize;
        if len > hard_limit {
            bail!("{what} length {len} exceeds maximum {hard_limit}");
        }
        if min_serialized_item_len > 0 {
            let max_from_payload =
                self.bytes.len().saturating_sub(self.offset) / min_serialized_item_len;
            if len > max_from_payload {
                bail!("{what} length {len} exceeds remaining payload capacity {max_from_payload}");
            }
        }
        Ok(len)
    }

    fn finish(&self) -> anyhow::Result<()> {
        if self.bytes[self.offset..].iter().any(|byte| *byte != 0) {
            bail!("trailing non-padding bytes after ros_discovery_info payload");
        }
        Ok(())
    }

    fn align(&mut self, alignment: usize) -> anyhow::Result<()> {
        let misalignment = self.offset % alignment;
        if misalignment == 0 {
            return Ok(());
        }
        let pad = alignment - misalignment;
        let next = self
            .offset
            .checked_add(pad)
            .context("CDR alignment overflow")?;
        if next > self.bytes.len() {
            bail!("CDR alignment out of bounds");
        }
        self.offset = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload(gid_len: usize) -> Vec<u8> {
        let mut payload = vec![0x00, 0x01, 0x00, 0x00];
        push_gid(&mut payload, 0x10, gid_len);
        payload.extend(1u32.to_le_bytes());
        push_cdr_string(&mut payload, "/demo");
        push_cdr_string(&mut payload, "talker");
        align(&mut payload);
        payload.extend(1u32.to_le_bytes());
        push_gid(&mut payload, 0x20, gid_len);
        align(&mut payload);
        payload.extend(1u32.to_le_bytes());
        push_gid(&mut payload, 0x30, gid_len);
        payload
    }

    fn push_gid(payload: &mut Vec<u8>, first: u8, gid_len: usize) {
        payload.extend(first..first + 16);
        payload.extend(std::iter::repeat(0).take(gid_len - 16));
    }

    fn push_cdr_string(payload: &mut Vec<u8>, value: &str) {
        align(payload);
        let len = value.len() + 1;
        payload.extend((len as u32).to_le_bytes());
        payload.extend(value.as_bytes());
        payload.push(0);
    }

    fn align(payload: &mut Vec<u8>) {
        while payload.len() % 4 != 0 {
            payload.push(0);
        }
    }

    #[test]
    fn parses_jazzy_ros_discovery_info_with_16_byte_gids() {
        let info = parse_participant_entities_info(&sample_payload(ROS_GID_LEN)).unwrap();

        assert_eq!(info.nodes.len(), 1);
        assert_eq!(info.nodes[0].node_namespace, "/demo");
        assert_eq!(info.nodes[0].node_name, "talker");
        assert_eq!(info.nodes[0].reader_gids.len(), 1);
        assert_eq!(info.nodes[0].writer_gids.len(), 1);
    }

    #[test]
    fn parses_humble_ros_discovery_info_with_24_byte_gids() {
        let info = parse_participant_entities_info(&sample_payload(ROS_GID_LEN_LEGACY)).unwrap();

        assert_eq!(info.nodes.len(), 1);
        assert_eq!(info.nodes[0].node_namespace, "/demo");
        assert_eq!(info.nodes[0].node_name, "talker");
        assert_eq!(info.nodes[0].reader_gids.len(), 1);
        assert_eq!(info.nodes[0].writer_gids.len(), 1);
    }

    #[test]
    fn rejects_unbounded_node_count_without_allocating() {
        let mut payload = vec![0x00, 0x01, 0x00, 0x00];
        push_gid(&mut payload, 0x10, ROS_GID_LEN);
        payload.extend(u32::MAX.to_le_bytes());

        let err = parse_participant_entities_info(&payload).unwrap_err();
        assert!(err.to_string().contains("node entity sequence length"));
    }

    #[test]
    fn rejects_unbounded_gid_sequence_without_allocating() {
        let mut payload = vec![0x00, 0x01, 0x00, 0x00];
        push_gid(&mut payload, 0x10, ROS_GID_LEN);
        payload.extend(1u32.to_le_bytes()); // one node
        push_cdr_string(&mut payload, "");
        push_cdr_string(&mut payload, "");
        payload.extend(u32::MAX.to_le_bytes()); // impossible reader_gid_seq
        payload.extend(0u32.to_le_bytes()); // enough remaining bytes to pass node-count check

        let err = parse_participant_entities_info(&payload).unwrap_err();
        assert!(err.to_string().contains("GID sequence length"));
    }
}
