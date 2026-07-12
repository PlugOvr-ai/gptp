//! Node name resolution: static names file, plus parsers for the protocols
//! we can learn names from — DHCP requests (option 12), mDNS (A and reverse
//! PTR records), and LLDP (system name TLV).

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{bail, Context, Result};

/// Static name mappings from the --names file. In a vehicle the network is
/// fixed, so curated names ("Front Radar") beat any discovered hostname;
/// they always take precedence.
#[derive(Default)]
pub struct StaticNames {
    pub by_mac: HashMap<[u8; 6], String>,
    pub by_ip: HashMap<Ipv4Addr, String>,
}

/// Load a names file. Format, one mapping per line:
///
/// ```text
/// # MAC or IPv4, '=' , display name
/// aa:bb:cc:dd:ee:ff = Front Radar
/// 10.0.0.7          = Lidar Roof
/// ```
pub fn load_names_file(path: &str) -> Result<StaticNames> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading names file {path}"))?;
    let mut names = StaticNames::default();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, name)) = line.split_once('=') else {
            bail!("{path}:{}: expected '<mac|ip> = <name>'", lineno + 1);
        };
        let key = key.trim();
        let name = name.trim().to_string();
        if name.is_empty() {
            bail!("{path}:{}: empty name", lineno + 1);
        }
        if let Some(mac) = parse_mac(key) {
            names.by_mac.insert(mac, name);
        } else if let Ok(ip) = key.parse::<Ipv4Addr>() {
            names.by_ip.insert(ip, name);
        } else {
            bail!("{path}:{}: {key} is neither a MAC nor an IPv4 address", lineno + 1);
        }
    }
    Ok(names)
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<u8> = s
        .split(':')
        .map(|p| u8::from_str_radix(p, 16))
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    parts.try_into().ok()
}

/// Extract (client MAC, hostname) from a DHCP client message (UDP payload).
pub fn parse_dhcp_hostname(p: &[u8]) -> Option<([u8; 6], String)> {
    // BOOTP: op=1 (request), fixed fields, then the DHCP magic cookie.
    if p.len() < 244 || p[0] != 1 || p[236..240] != [0x63, 0x82, 0x53, 0x63] {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&p[28..34]);
    let mut off = 240;
    while off + 1 < p.len() {
        match p[off] {
            255 => break,
            0 => off += 1,
            code => {
                let len = *p.get(off + 1)? as usize;
                let data = p.get(off + 2..off + 2 + len)?;
                if code == 12 {
                    return clean_name(&String::from_utf8_lossy(data)).map(|n| (mac, n));
                }
                off += 2 + len;
            }
        }
    }
    None
}

/// Extract (IP, hostname) pairs from a DNS/mDNS message: A records give
/// name→address directly; PTR records in in-addr.arpa give address→name
/// (these answer our reverse queries).
pub fn parse_dns_names(p: &[u8]) -> Vec<(Ipv4Addr, String)> {
    parse_dns_inner(p).unwrap_or_default()
}

fn parse_dns_inner(p: &[u8]) -> Option<Vec<(Ipv4Addr, String)>> {
    if p.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([p[4], p[5]]);
    let rrcount = u16::from_be_bytes([p[6], p[7]]) as u32
        + u16::from_be_bytes([p[8], p[9]]) as u32
        + u16::from_be_bytes([p[10], p[11]]) as u32;
    let mut off = 12;
    for _ in 0..qdcount {
        let (_, next) = decode_dns_name(p, off)?;
        off = next + 4; // qtype + qclass
    }
    let mut out = Vec::new();
    for _ in 0..rrcount.min(64) {
        let (name, next) = decode_dns_name(p, off)?;
        let rtype = u16::from_be_bytes([*p.get(next)?, *p.get(next + 1)?]);
        let rdlen =
            u16::from_be_bytes([*p.get(next + 8)?, *p.get(next + 9)?]) as usize;
        let rdata_off = next + 10;
        let rdata = p.get(rdata_off..rdata_off + rdlen)?;
        match rtype {
            1 if rdlen == 4 => {
                // A record: name has this address.
                if let Some(n) = clean_name(&name) {
                    out.push((Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]), n));
                }
            }
            12 => {
                // PTR: reverse-zone name encodes the address.
                if let Some(ip) = reverse_arpa_to_ip(&name) {
                    let (target, _) = decode_dns_name(p, rdata_off)?;
                    if let Some(n) = clean_name(&target) {
                        out.push((ip, n));
                    }
                }
            }
            _ => {}
        }
        off = rdata_off + rdlen;
    }
    Some(out)
}

/// Decode a (possibly compressed) DNS name; returns the name and the offset
/// just past its encoding at the original position.
fn decode_dns_name(p: &[u8], mut off: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let b = *p.get(off)?;
        if b & 0xC0 == 0xC0 {
            let ptr = (((b & 0x3F) as usize) << 8) | *p.get(off + 1)? as usize;
            if end.is_none() {
                end = Some(off + 2);
            }
            jumps += 1;
            if jumps > 16 || ptr >= off {
                return None;
            }
            off = ptr;
        } else if b == 0 {
            return Some((out, end.unwrap_or(off + 1)));
        } else {
            let len = b as usize;
            let label = p.get(off + 1..off + 1 + len)?;
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(&String::from_utf8_lossy(label));
            if out.len() > 255 {
                return None;
            }
            off += 1 + len;
        }
    }
}

fn reverse_arpa_to_ip(name: &str) -> Option<Ipv4Addr> {
    let rest = name.strip_suffix(".in-addr.arpa")?;
    let parts: Vec<u8> = rest
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let [d, c, b, a] = parts.try_into().ok()?;
    Some(Ipv4Addr::new(a, b, c, d))
}

/// Build an mDNS reverse-PTR query (QM: responders answer to the multicast
/// group, where our passive capture picks the answer up).
pub fn build_mdns_ptr_query(ip: Ipv4Addr) -> Vec<u8> {
    let mut q = vec![0u8; 12];
    q[5] = 1; // qdcount = 1
    let o = ip.octets();
    for label in [
        o[3].to_string(),
        o[2].to_string(),
        o[1].to_string(),
        o[0].to_string(),
        "in-addr".to_string(),
        "arpa".to_string(),
    ] {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&12u16.to_be_bytes()); // PTR
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    q
}

/// Extract the system name from an LLDP frame body (after the Ethernet
/// header). LLDP is link-local, so in practice this names the switch.
pub fn parse_lldp_sysname(p: &[u8]) -> Option<String> {
    let mut off = 0;
    while off + 2 <= p.len() {
        let hdr = u16::from_be_bytes([p[off], p[off + 1]]);
        let tlv_type = (hdr >> 9) as u8;
        let len = (hdr & 0x01FF) as usize;
        if tlv_type == 0 {
            break;
        }
        let data = p.get(off + 2..off + 2 + len)?;
        if tlv_type == 5 {
            return clean_name(&String::from_utf8_lossy(data));
        }
        off += 2 + len;
    }
    None
}

/// Normalize a discovered name for display: drop the .local suffix,
/// control characters, and excessive length.
fn clean_name(s: &str) -> Option<String> {
    let s = s.strip_suffix(".local").unwrap_or(s);
    let s: String = s
        .trim_matches('.')
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(48)
        .collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_file_parsing() {
        let dir = std::env::temp_dir().join("gptp-names-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("names.txt");
        std::fs::write(
            &path,
            "# comment\naa:bb:cc:dd:ee:ff = Front Radar\n10.0.0.7 = Lidar Roof\n\n",
        )
        .unwrap();
        let names = load_names_file(path.to_str().unwrap()).unwrap();
        assert_eq!(
            names.by_mac[&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]],
            "Front Radar"
        );
        assert_eq!(names.by_ip[&Ipv4Addr::new(10, 0, 0, 7)], "Lidar Roof");

        std::fs::write(&path, "not-a-key = X\n").unwrap();
        assert!(load_names_file(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn dhcp_hostname() {
        let mut p = vec![0u8; 240];
        p[0] = 1; // BOOTREQUEST
        p[28..34].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03]);
        p[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        p.extend_from_slice(&[53, 1, 1]); // message type: discover
        p.extend_from_slice(&[12, 5]); // option 12, len 5
        p.extend_from_slice(b"radar");
        p.push(255);
        // pad to the 244-byte minimum handled by real clients
        p.resize(p.len().max(244), 0);
        let (mac, name) = parse_dhcp_hostname(&p).unwrap();
        assert_eq!(mac, [0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03]);
        assert_eq!(name, "radar");
    }

    fn dns_labels(name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        for label in name.split('.') {
            v.push(label.len() as u8);
            v.extend_from_slice(label.as_bytes());
        }
        v.push(0);
        v
    }

    #[test]
    fn dns_a_record() {
        let mut p = vec![0u8; 12];
        p[7] = 1; // ancount = 1
        p.extend(dns_labels("pi.local"));
        p.extend_from_slice(&1u16.to_be_bytes()); // A
        p.extend_from_slice(&1u16.to_be_bytes()); // IN
        p.extend_from_slice(&120u32.to_be_bytes()); // ttl
        p.extend_from_slice(&4u16.to_be_bytes());
        p.extend_from_slice(&[10, 0, 0, 2]);
        assert_eq!(
            parse_dns_names(&p),
            vec![(Ipv4Addr::new(10, 0, 0, 2), "pi".to_string())]
        );
    }

    #[test]
    fn dns_reverse_ptr_with_compression() {
        let mut p = vec![0u8; 12];
        p[5] = 1; // qdcount = 1
        p[7] = 1; // ancount = 1
        let name_off = p.len() as u16;
        p.extend(dns_labels("2.0.0.10.in-addr.arpa"));
        p.extend_from_slice(&12u16.to_be_bytes());
        p.extend_from_slice(&1u16.to_be_bytes());
        // answer: compression pointer back to the question name
        p.extend_from_slice(&(0xC000 | name_off).to_be_bytes());
        p.extend_from_slice(&12u16.to_be_bytes()); // PTR
        p.extend_from_slice(&1u16.to_be_bytes()); // IN
        p.extend_from_slice(&120u32.to_be_bytes());
        let rdata = dns_labels("front-lidar.local");
        p.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        p.extend(rdata);
        assert_eq!(
            parse_dns_names(&p),
            vec![(Ipv4Addr::new(10, 0, 0, 2), "front-lidar".to_string())]
        );
    }

    #[test]
    fn dns_rejects_pointer_loops() {
        let mut p = vec![0u8; 12];
        p[7] = 1;
        // name that points at itself
        p.extend_from_slice(&[0xC0, 12]);
        p.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert!(parse_dns_names(&p).is_empty());
    }

    #[test]
    fn mdns_query_roundtrip() {
        // Our own parser must survive (and ignore) our own query packets.
        let q = build_mdns_ptr_query(Ipv4Addr::new(10, 0, 0, 2));
        assert!(parse_dns_names(&q).is_empty());
        let (name, _) = decode_dns_name(&q, 12).unwrap();
        assert_eq!(name, "2.0.0.10.in-addr.arpa");
    }

    #[test]
    fn lldp_system_name() {
        let mut p = Vec::new();
        // chassis id TLV (type 1)
        p.extend_from_slice(&((1u16 << 9) | 2).to_be_bytes());
        p.extend_from_slice(&[4, 0xAB]);
        // system name TLV (type 5)
        p.extend_from_slice(&((5u16 << 9) | 3).to_be_bytes());
        p.extend_from_slice(b"sw1");
        // end TLV
        p.extend_from_slice(&[0, 0]);
        assert_eq!(parse_lldp_sysname(&p).unwrap(), "sw1");
    }
}
