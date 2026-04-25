use aya_ebpf::programs::SkBuffContext;
use ros2probe_common::TOPIC_GID_STATE_AVAILABLE;

use crate::{
    maps::{self, FragmentFlowKey},
    rtps,
};

const ETH_HDR_LEN: usize = 14;
const VLAN_HDR_LEN: usize = 4;
const ETH_P_IPV4: u16 = 0x0800;
const ETH_P_IPV6: u16 = 0x86dd;
const ETH_P_8021Q: u16 = 0x8100;
const ETH_P_8021AD: u16 = 0x88a8;
const ETH_P_8021QINQ: u16 = 0x9100;
const IPV4_MIN_HDR_LEN: usize = 20;
const IPV6_HDR_LEN: usize = 40;
const IPV4_FLAG_MORE_FRAGMENTS: u16 = 0x2000;
const IPV4_FRAGMENT_OFFSET_MASK: u16 = 0x1fff;
const IPPROTO_UDP: u8 = 17;
const UDP_HDR_LEN: usize = 8;
const IPV6_NEXT_HEADER_HOP_BY_HOP: u8 = 0;
const IPV6_NEXT_HEADER_ROUTING: u8 = 43;
const IPV6_NEXT_HEADER_FRAGMENT: u8 = 44;
const IPV6_NEXT_HEADER_ESP: u8 = 50;
const IPV6_NEXT_HEADER_AUTH: u8 = 51;
const IPV6_NEXT_HEADER_DESTINATION: u8 = 60;
const IPV6_NEXT_HEADER_NO_NEXT: u8 = 59;

#[derive(Clone, Copy)]
struct IpPacket {
    l4_offset: usize,
    fragmented: bool,
    /// Raw fragment-offset field in **8-byte blocks** (both IPv4 and IPv6 use
    /// 8-byte-unit offsets). `0` for the first fragment of a datagram or for
    /// non-fragmented packets. Only ever compared to `0`; if a future change
    /// needs it in bytes, multiply by 8.
    fragment_offset: u16,
    more_fragments: bool,
    /// Key identifying the fragmented datagram this packet belongs to. Only
    /// meaningful when `fragmented == true`; zero-filled otherwise.
    frag_key: FragmentFlowKey,
}

#[derive(Clone, Copy)]
struct UdpHeader;

pub fn run(ctx: &SkBuffContext) -> Result<i64, i64> {
    let packet = match parse_ip_packet(ctx)? {
        Some(packet) => packet,
        None => return Ok(0),
    };

    // Middle / trailing fragment (offset > 0): no UDP or RTPS header is
    // present, so we cannot classify the payload. Decide purely by the
    // per-datagram approval map that the first fragment populated.
    if packet.fragmented && packet.fragment_offset > 0 {
        let approved = maps::frag_flow_approved(&packet.frag_key);
        if approved && !packet.more_fragments {
            // Last fragment of an approved datagram — clean up the entry.
            maps::frag_flow_forget(&packet.frag_key);
        }
        return if approved { Ok(ctx.len() as i64) } else { Ok(0) };
    }

    // Non-fragmented datagram OR first fragment of a fragmented datagram.
    // Both carry the UDP + RTPS headers at the expected offsets, so the
    // classifier can inspect them normally.
    let _udp = match parse_udp(ctx, &packet)? {
        Some(udp) => udp,
        None => return Ok(0),
    };

    let payload_offset = packet.l4_offset + UDP_HDR_LEN;
    let route = match rtps::classify(ctx, payload_offset)? {
        Some(route) => route,
        None => return Ok(0),
    };

    let pass_to_userspace = if route.is_discovery {
        true
    } else if !route.has_writer_gid && !route.has_reader_gid {
        false
    } else {
        matches!(route.has_writer_gid, true)
            && matches!(maps::gid_state(&route.writer_gid), Some(TOPIC_GID_STATE_AVAILABLE))
            || matches!(route.has_reader_gid, true)
                && matches!(maps::gid_state(&route.reader_gid), Some(TOPIC_GID_STATE_AVAILABLE))
    };

    // If this is the first fragment of a soon-to-be-split datagram and we
    // just decided to pass it, remember the fragment key so the trailing
    // fragments can be waved through on their own (headerless) visits.
    if pass_to_userspace && packet.fragmented {
        maps::frag_flow_mark(&packet.frag_key);
    }

    if pass_to_userspace {
        Ok(ctx.len() as i64)
    } else {
        Ok(0)
    }
}

fn parse_ip_packet(ctx: &SkBuffContext) -> Result<Option<IpPacket>, i64> {
    let Some((ethertype, l3_offset)) = parse_l3_offset(ctx)? else {
        return Ok(None);
    };

    match ethertype {
        ETH_P_IPV4 => parse_ipv4(ctx, l3_offset),
        ETH_P_IPV6 => parse_ipv6(ctx, l3_offset),
        _ => Ok(None),
    }
}

fn parse_ipv4(ctx: &SkBuffContext, l3_offset: usize) -> Result<Option<IpPacket>, i64> {
    let version_ihl = ctx.load::<u8>(l3_offset).map_err(|err| err as i64)?;
    let version = version_ihl >> 4;
    if version != 4 {
        return Ok(None);
    }

    let ihl_words = (version_ihl & 0x0f) as usize;
    if ihl_words < 5 {
        return Ok(None);
    }

    let header_len = ihl_words * 4;
    if header_len < IPV4_MIN_HDR_LEN {
        return Ok(None);
    }

    let protocol = ctx.load::<u8>(l3_offset + 9).map_err(|err| err as i64)?;
    if protocol != IPPROTO_UDP {
        return Ok(None);
    }

    let identification =
        u16::from_be(ctx.load::<u16>(l3_offset + 4).map_err(|err| err as i64)?);
    let fragment_bits =
        u16::from_be(ctx.load::<u16>(l3_offset + 6).map_err(|err| err as i64)?);
    let fragment_offset = fragment_bits & IPV4_FRAGMENT_OFFSET_MASK;
    let more_fragments = (fragment_bits & IPV4_FLAG_MORE_FRAGMENTS) != 0;
    let fragmented = fragment_offset != 0 || more_fragments;

    // Read addresses as 4-byte words; `to_ne_bytes()` preserves the packet
    // byte order (the `u32` was loaded directly from memory on a bpfel host).
    let src_raw = ctx.load::<u32>(l3_offset + 12).map_err(|err| err as i64)?;
    let dst_raw = ctx.load::<u32>(l3_offset + 16).map_err(|err| err as i64)?;
    let src_bytes = src_raw.to_ne_bytes();
    let dst_bytes = dst_raw.to_ne_bytes();

    let mut src_ip = [0u8; 16];
    let mut dst_ip = [0u8; 16];
    src_ip[0] = src_bytes[0];
    src_ip[1] = src_bytes[1];
    src_ip[2] = src_bytes[2];
    src_ip[3] = src_bytes[3];
    dst_ip[0] = dst_bytes[0];
    dst_ip[1] = dst_bytes[1];
    dst_ip[2] = dst_bytes[2];
    dst_ip[3] = dst_bytes[3];

    Ok(Some(IpPacket {
        l4_offset: l3_offset + header_len,
        fragmented,
        fragment_offset,
        more_fragments,
        frag_key: FragmentFlowKey {
            src_ip,
            dst_ip,
            identification: identification as u32,
            protocol: protocol as u32,
        },
    }))
}

fn parse_ipv6(ctx: &SkBuffContext, l3_offset: usize) -> Result<Option<IpPacket>, i64> {
    let version = ctx.load::<u8>(l3_offset).map_err(|err| err as i64)? >> 4;
    if version != 6 {
        return Ok(None);
    }

    let _payload_len = u16::from_be(ctx.load::<u16>(l3_offset + 4).map_err(|err| err as i64)?);
    let src_ip = read_ipv6_addr(ctx, l3_offset + 8)?;
    let dst_ip = read_ipv6_addr(ctx, l3_offset + 24)?;

    let mut next_header = ctx.load::<u8>(l3_offset + 6).map_err(|err| err as i64)?;
    let mut offset = l3_offset + IPV6_HDR_LEN;

    for _ in 0..4 {
        match next_header {
            IPPROTO_UDP => {
                return Ok(Some(IpPacket {
                    l4_offset: offset,
                    fragmented: false,
                    fragment_offset: 0,
                    more_fragments: false,
                    frag_key: FragmentFlowKey {
                        src_ip,
                        dst_ip,
                        identification: 0,
                        protocol: IPPROTO_UDP as u32,
                    },
                }));
            }
            IPV6_NEXT_HEADER_HOP_BY_HOP
            | IPV6_NEXT_HEADER_ROUTING
            | IPV6_NEXT_HEADER_DESTINATION => {
                let ext_next = ctx.load::<u8>(offset).map_err(|err| err as i64)?;
                let ext_len = ctx.load::<u8>(offset + 1).map_err(|err| err as i64)?;
                next_header = ext_next;
                offset += (ext_len as usize + 1) * 8;
            }
            IPV6_NEXT_HEADER_FRAGMENT => {
                let ext_next = ctx.load::<u8>(offset).map_err(|err| err as i64)?;
                let fragment_bits =
                    u16::from_be(ctx.load::<u16>(offset + 2).map_err(|err| err as i64)?);
                let identification =
                    u32::from_be(ctx.load::<u32>(offset + 4).map_err(|err| err as i64)?);
                let fragment_offset = fragment_bits >> 3;
                let more_fragments = (fragment_bits & 0x1) != 0;
                let fragmented = fragment_offset != 0 || more_fragments;

                next_header = ext_next;
                offset += 8;

                if fragmented {
                    return Ok(Some(IpPacket {
                        l4_offset: offset,
                        fragmented: true,
                        fragment_offset,
                        more_fragments,
                        frag_key: FragmentFlowKey {
                            src_ip,
                            dst_ip,
                            identification,
                            protocol: ext_next as u32,
                        },
                    }));
                }
                // Non-fragmented packet with a Fragment extension header
                // (offset=0 and MF=0). Keep walking headers to reach UDP.
            }
            IPV6_NEXT_HEADER_AUTH => {
                let ext_next = ctx.load::<u8>(offset).map_err(|err| err as i64)?;
                let ext_len = ctx.load::<u8>(offset + 1).map_err(|err| err as i64)?;
                next_header = ext_next;
                offset += (ext_len as usize + 2) * 4;
            }
            IPV6_NEXT_HEADER_ESP | IPV6_NEXT_HEADER_NO_NEXT => return Ok(None),
            _ => return Ok(None),
        }
    }

    Ok(None)
}

fn read_ipv6_addr(ctx: &SkBuffContext, offset: usize) -> Result<[u8; 16], i64> {
    let w0 = ctx.load::<u32>(offset).map_err(|err| err as i64)?;
    let w1 = ctx.load::<u32>(offset + 4).map_err(|err| err as i64)?;
    let w2 = ctx.load::<u32>(offset + 8).map_err(|err| err as i64)?;
    let w3 = ctx.load::<u32>(offset + 12).map_err(|err| err as i64)?;
    let b0 = w0.to_ne_bytes();
    let b1 = w1.to_ne_bytes();
    let b2 = w2.to_ne_bytes();
    let b3 = w3.to_ne_bytes();
    Ok([
        b0[0], b0[1], b0[2], b0[3],
        b1[0], b1[1], b1[2], b1[3],
        b2[0], b2[1], b2[2], b2[3],
        b3[0], b3[1], b3[2], b3[3],
    ])
}

fn parse_udp(ctx: &SkBuffContext, packet: &IpPacket) -> Result<Option<UdpHeader>, i64> {
    let udp_offset = packet.l4_offset;
    let length = u16::from_be(ctx.load::<u16>(udp_offset + 4).map_err(|err| err as i64)?);

    if length < UDP_HDR_LEN as u16 {
        return Ok(None);
    }

    let _ = length;

    Ok(Some(UdpHeader))
}

fn parse_l3_offset(ctx: &SkBuffContext) -> Result<Option<(u16, usize)>, i64> {
    let mut offset = ETH_HDR_LEN;
    let mut ethertype = u16::from_be(ctx.load::<u16>(12).map_err(|err| err as i64)?);

    let mut remaining_tags = 2;
    while remaining_tags > 0
        && matches!(ethertype, ETH_P_8021Q | ETH_P_8021AD | ETH_P_8021QINQ)
    {
        ethertype = u16::from_be(
            ctx.load::<u16>(offset + 2)
                .map_err(|err| err as i64)?,
        );
        offset += VLAN_HDR_LEN;
        remaining_tags -= 1;
    }

    if ethertype != ETH_P_IPV4 {
        if ethertype != ETH_P_IPV6 {
            return Ok(None);
        }
    }

    Ok(Some((ethertype, offset)))
}
