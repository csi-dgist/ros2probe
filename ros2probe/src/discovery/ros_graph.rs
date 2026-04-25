use anyhow::{Context, bail};
use ros2probe_common::TopicGid;

use crate::discovery::NodeSample;

const CDR_BE: u16 = 0x0000;
const CDR_LE: u16 = 0x0001;
const ROS_DISCOVERY_INFO_TOPIC: &str = "/ros_discovery_info";
const ROS_DISCOVERY_INFO_TYPE: &str = "rmw_dds_common/msg/ParticipantEntitiesInfo";
const ROS_GID_LEN: usize = 24;

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

    let mut reader = CdrReader::new(&payload[4..], little_endian);
    let participant_gid = reader.read_topic_gid()?;
    let node_count = reader.read_u32()? as usize;
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let node_namespace = reader.read_string()?;
        let node_name = reader.read_string()?;
        let reader_gids = reader.read_gid_sequence()?;
        let writer_gids = reader.read_gid_sequence()?;
        nodes.push(NodeSample {
            participant_gid,
            node_namespace,
            node_name,
            writer_gids,
            reader_gids,
        });
    }

    Ok(ParticipantEntitiesInfo {
        participant_gid,
        nodes,
    })
}

struct CdrReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    little_endian: bool,
}

impl<'a> CdrReader<'a> {
    fn new(bytes: &'a [u8], little_endian: bool) -> Self {
        Self {
            bytes,
            offset: 0,
            little_endian,
        }
    }

    fn read_u32(&mut self) -> anyhow::Result<u32> {
        self.align(4)?;
        let bytes = self
            .bytes
            .get(self.offset..self.offset + 4)
            .context("u32 out of bounds")?;
        self.offset += 4;
        Ok(if self.little_endian {
            u32::from_le_bytes(bytes.try_into().unwrap())
        } else {
            u32::from_be_bytes(bytes.try_into().unwrap())
        })
    }

    fn read_string(&mut self) -> anyhow::Result<String> {
        self.align(4)?;
        let len = self.read_u32()? as usize;
        if len == 0 {
            return Ok(String::new());
        }

        let bytes = self
            .bytes
            .get(self.offset..self.offset + len)
            .context("string bytes out of bounds")?;
        self.offset += len;
        let string_bytes = bytes
            .strip_suffix(&[0])
            .context("CDR string missing trailing null terminator")?;
        Ok(std::str::from_utf8(string_bytes)
            .context("decode UTF-8 string")?
            .to_string())
    }

    fn read_gid_sequence(&mut self) -> anyhow::Result<Vec<TopicGid>> {
        self.align(4)?;
        let len = self.read_u32()? as usize;
        let mut gids = Vec::with_capacity(len);
        for _ in 0..len {
            gids.push(self.read_topic_gid()?);
        }
        Ok(gids)
    }

    fn read_topic_gid(&mut self) -> anyhow::Result<TopicGid> {
        let bytes = self
            .bytes
            .get(self.offset..self.offset + ROS_GID_LEN)
            .context("GID bytes out of bounds")?;
        self.offset += ROS_GID_LEN;

        let mut gid = [0u8; 16];
        gid.copy_from_slice(&bytes[..16]);
        Ok(TopicGid::new(gid))
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
