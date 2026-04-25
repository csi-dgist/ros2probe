pub const TOPIC_GID_LEN: usize = 16;
pub const IP_ADDR_LEN: usize = 16;
pub const IP_FAMILY_V4: u8 = 4;
pub const IP_FAMILY_V6: u8 = 6;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct IpAddr {
    pub family: u8,
    pub _pad: [u8; 3],
    pub bytes: [u8; IP_ADDR_LEN],
}

impl IpAddr {
    pub const fn from_v4(v4: u32) -> Self {
        let octets = v4.to_be_bytes();
        let mut bytes = [0u8; IP_ADDR_LEN];
        bytes[0] = octets[0];
        bytes[1] = octets[1];
        bytes[2] = octets[2];
        bytes[3] = octets[3];
        Self {
            family: IP_FAMILY_V4,
            _pad: [0; 3],
            bytes,
        }
    }

    pub const fn from_v6(bytes: [u8; IP_ADDR_LEN]) -> Self {
        Self {
            family: IP_FAMILY_V6,
            _pad: [0; 3],
            bytes,
        }
    }

    pub const fn is_v4(&self) -> bool {
        self.family == IP_FAMILY_V4
    }

    pub const fn is_v6(&self) -> bool {
        self.family == IP_FAMILY_V6
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TopicGid {
    pub bytes: [u8; TOPIC_GID_LEN],
}

impl TopicGid {
    pub const fn new(bytes: [u8; TOPIC_GID_LEN]) -> Self {
        Self { bytes }
    }

    pub fn from_rtps_parts(guid_prefix: [u8; 12], writer_entity_id: [u8; 4]) -> Self {
        let mut bytes = [0u8; TOPIC_GID_LEN];
        bytes[..12].copy_from_slice(&guid_prefix);
        bytes[12..].copy_from_slice(&writer_entity_id);
        Self { bytes }
    }

    pub const fn guid_prefix(&self) -> [u8; 12] {
        [
            self.bytes[0],
            self.bytes[1],
            self.bytes[2],
            self.bytes[3],
            self.bytes[4],
            self.bytes[5],
            self.bytes[6],
            self.bytes[7],
            self.bytes[8],
            self.bytes[9],
            self.bytes[10],
            self.bytes[11],
        ]
    }

    pub const fn writer_entity_id(&self) -> [u8; 4] {
        [self.bytes[12], self.bytes[13], self.bytes[14], self.bytes[15]]
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FlowTuple {
    pub src_ip: IpAddr,
    pub dst_ip: IpAddr,
    pub src_port: u16,
    pub dst_port: u16,
}

impl FlowTuple {
    pub const fn new(src_ip: IpAddr, dst_ip: IpAddr, src_port: u16, dst_port: u16) -> Self {
        Self {
            src_ip,
            dst_ip,
            src_port,
            dst_port,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Ipv4FragmentKey {
    pub src_ip: u32,
    pub dst_ip: u32,
    pub identification: u16,
    pub protocol: u8,
    pub _pad: u8,
}

impl Ipv4FragmentKey {
    pub const fn new(src_ip: u32, dst_ip: u32, identification: u16, protocol: u8) -> Self {
        Self {
            src_ip,
            dst_ip,
            identification,
            protocol,
            _pad: 0,
        }
    }
}
