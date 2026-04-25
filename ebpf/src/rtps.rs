use aya_ebpf::programs::SkBuffContext;
use ros2probe_common::{
    TopicGid, RTPS_GUID_PREFIX_LEN, RTPS_GUID_PREFIX_OFFSET, RTPS_HEADER_LEN, RTPS_SIGNATURE,
    RTPS_SIGNATURE_OFFSET, RTPS_SUBMESSAGE_DATA, RTPS_SUBMESSAGE_DATA_FRAG,
    RTPS_WRITER_ENTITY_ID_LEN, RTPS_WRITER_ENTITY_ID_SEDP_PUBLICATIONS,
    RTPS_WRITER_ENTITY_ID_SEDP_SUBSCRIPTIONS, RTPS_WRITER_ENTITY_ID_SPDP_PARTICIPANT,
};

const RTPS_SCAN_SUBMESSAGES: usize = 4;
const RTPS_SUBMESSAGE_HEADER_LEN: usize = 4;
const RTPS_SUBMESSAGE_ENDIAN_FLAG: u8 = 0x01;
const RTPS_SUBMESSAGE_READER_ID_OFFSET: usize = 8;
const RTPS_SUBMESSAGE_WRITER_ID_OFFSET: usize = 12;

#[derive(Clone, Copy)]
pub struct RtpsRoute {
    pub writer_gid: TopicGid,
    pub reader_gid: TopicGid,
    pub has_writer_gid: bool,
    pub has_reader_gid: bool,
    pub is_discovery: bool,
}

pub fn classify(ctx: &SkBuffContext, payload_offset: usize) -> Result<Option<RtpsRoute>, i64> {
    if !has_signature(ctx, payload_offset)? {
        return Ok(None);
    }

    // Load the 12-byte GUID prefix as 3x u32 (3 helper calls instead of 12).
    let w0 = ctx
        .load::<u32>(payload_offset + RTPS_GUID_PREFIX_OFFSET)
        .map_err(|err| err as i64)?;
    let w1 = ctx
        .load::<u32>(payload_offset + RTPS_GUID_PREFIX_OFFSET + 4)
        .map_err(|err| err as i64)?;
    let w2 = ctx
        .load::<u32>(payload_offset + RTPS_GUID_PREFIX_OFFSET + 8)
        .map_err(|err| err as i64)?;
    let b0 = w0.to_ne_bytes();
    let b1 = w1.to_ne_bytes();
    let b2 = w2.to_ne_bytes();
    let guid_prefix: [u8; RTPS_GUID_PREFIX_LEN] = [
        b0[0], b0[1], b0[2], b0[3], b1[0], b1[1], b1[2], b1[3], b2[0], b2[1], b2[2], b2[3],
    ];

    let mut submessage_offset = payload_offset + RTPS_HEADER_LEN;
    let mut scan_count = 0usize;
    while scan_count < RTPS_SCAN_SUBMESSAGES {
        let submessage_id = ctx.load::<u8>(submessage_offset).map_err(|err| err as i64)?;
        let flags = ctx.load::<u8>(submessage_offset + 1).map_err(|err| err as i64)?;
        let octets_0 = ctx.load::<u8>(submessage_offset + 2).map_err(|err| err as i64)?;
        let octets_1 = ctx.load::<u8>(submessage_offset + 3).map_err(|err| err as i64)?;

        if submessage_id == RTPS_SUBMESSAGE_DATA || submessage_id == RTPS_SUBMESSAGE_DATA_FRAG {
            let reader_entity_id = load_entity_id(ctx, submessage_offset, RTPS_SUBMESSAGE_READER_ID_OFFSET)?;
            let writer_entity_id = load_writer_entity_id(ctx, submessage_offset)?;
            let writer_gid = TopicGid::from_rtps_parts(guid_prefix, writer_entity_id);
            let reader_gid = TopicGid::from_rtps_parts(guid_prefix, reader_entity_id);
            return Ok(Some(RtpsRoute {
                writer_gid,
                reader_gid,
                has_writer_gid: true,
                has_reader_gid: true,
                is_discovery: is_discovery_writer(&writer_entity_id),
            }));
        }

        let octets_to_next_header = if (flags & RTPS_SUBMESSAGE_ENDIAN_FLAG) != 0 {
            u16::from_le_bytes([octets_0, octets_1])
        } else {
            u16::from_be_bytes([octets_0, octets_1])
        } as usize;
        if octets_to_next_header == 0 {
            break;
        }

        submessage_offset += RTPS_SUBMESSAGE_HEADER_LEN + octets_to_next_header;
        scan_count += 1;
    }

    Ok(Some(RtpsRoute {
        writer_gid: TopicGid::new([0; 16]),
        reader_gid: TopicGid::new([0; 16]),
        has_writer_gid: false,
        has_reader_gid: false,
        is_discovery: false,
    }))
}

fn has_signature(ctx: &SkBuffContext, payload_offset: usize) -> Result<bool, i64> {
    // Load the 4-byte "RTPS" signature as one u32 (1 helper call instead of 4).
    const EXPECTED: u32 = u32::from_ne_bytes(RTPS_SIGNATURE);
    let word = ctx
        .load::<u32>(payload_offset + RTPS_SIGNATURE_OFFSET)
        .map_err(|err| err as i64)?;
    Ok(word == EXPECTED)
}

fn load_writer_entity_id(
    ctx: &SkBuffContext,
    submessage_offset: usize,
) -> Result<[u8; RTPS_WRITER_ENTITY_ID_LEN], i64> {
    load_entity_id(ctx, submessage_offset, RTPS_SUBMESSAGE_WRITER_ID_OFFSET)
}

fn load_entity_id(
    ctx: &SkBuffContext,
    submessage_offset: usize,
    entity_id_offset: usize,
) -> Result<[u8; RTPS_WRITER_ENTITY_ID_LEN], i64> {
    // Single u32 read instead of 4 byte reads.
    let word = ctx
        .load::<u32>(submessage_offset + entity_id_offset)
        .map_err(|err| err as i64)?;
    Ok(word.to_ne_bytes())
}

fn is_discovery_writer(writer_entity_id: &[u8; 4]) -> bool {
    *writer_entity_id == RTPS_WRITER_ENTITY_ID_SPDP_PARTICIPANT
        || *writer_entity_id == RTPS_WRITER_ENTITY_ID_SEDP_PUBLICATIONS
        || *writer_entity_id == RTPS_WRITER_ENTITY_ID_SEDP_SUBSCRIPTIONS
}
