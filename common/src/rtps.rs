pub const RTPS_SIGNATURE: [u8; 4] = *b"RTPS";
pub const RTPS_HEADER_LEN: usize = 20;
pub const RTPS_SIGNATURE_OFFSET: usize = 0;
pub const RTPS_VERSION_OFFSET: usize = 4;
pub const RTPS_VENDOR_ID_OFFSET: usize = 6;
pub const RTPS_GUID_PREFIX_OFFSET: usize = 8;
pub const RTPS_GUID_PREFIX_LEN: usize = 12;
pub const RTPS_WRITER_ENTITY_ID_LEN: usize = 4;

pub const RTPS_SUBMESSAGE_INFO_TS: u8 = 0x09;
pub const RTPS_SUBMESSAGE_DATA: u8 = 0x15;
pub const RTPS_SUBMESSAGE_DATA_FRAG: u8 = 0x16;
pub const RTPS_SUBMESSAGE_HEARTBEAT: u8 = 0x07;
pub const RTPS_SUBMESSAGE_HEARTBEAT_FRAG: u8 = 0x13;
pub const RTPS_SUBMESSAGE_ACKNACK: u8 = 0x06;
pub const RTPS_SUBMESSAGE_NACK_FRAG: u8 = 0x12;
pub const RTPS_SUBMESSAGE_GAP: u8 = 0x08;

pub const RTPS_INFO_TS_INVALIDATE_FLAG: u8 = 0x02;

pub const RTPS_WRITER_ENTITY_ID_SPDP_PARTICIPANT: [u8; 4] = [0x00, 0x01, 0x00, 0xc2];
pub const RTPS_WRITER_ENTITY_ID_SEDP_PUBLICATIONS: [u8; 4] = [0x00, 0x00, 0x03, 0xc2];
pub const RTPS_WRITER_ENTITY_ID_SEDP_SUBSCRIPTIONS: [u8; 4] = [0x00, 0x00, 0x04, 0xc2];

// DDS participant entity ID (GUID suffix for participant discovery entries).
pub const RTPS_PARTICIPANT_ENTITY_ID: [u8; 4] = [0x00, 0x00, 0x01, 0xc1];
