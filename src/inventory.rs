//! Network node inventory: passive MAC/IP learning plus active latency
//! probing (ICMP echo, falling back to ARP-request RTT).
//!
//! This answers "who is on the network, is it reachable, and is its path
//! congested". RTT and jitter here are *network/stack* latency — deliberately
//! not a statement about a node's gPTP sync accuracy, which silent slaves do
//! not expose. Jitter spikes correlate with switch queue congestion, which is
//! also what inflates gPTP residence times.

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::names::{self, StaticNames};
use crate::state::{NameSource, NodeInfo, ProbeMethod, Status};

const ETH_P_ALL: u16 = 0x0003;
const ETH_P_ARP: u16 = 0x0806;
const ETH_P_IPV4: u16 = 0x0800;
const ETH_P_LLDP: u16 = 0x88CC;

const MDNS_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;
const NAME_QUERY_INTERVAL: Duration = Duration::from_secs(60);

const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
const MAX_NODES: usize = 256;

struct Node {
    info: NodeInfo,
    /// In-flight probe tick; finalized when the next tick starts.
    tick: Option<Tick>,
    icmp_seq: u16,
    last_name_query: Option<Instant>,
}

/// One probe round: ICMP echo + ARP request sent together. Replies fill in
/// their RTTs independently; finalize() prefers ICMP (it traverses the
/// node's IP stack) and falls back to ARP.
struct Tick {
    sent_at: Instant,
    icmp_rtt_us: Option<f64>,
    arp_rtt_us: Option<f64>,
}

pub fn run(
    iface: String,
    our_mac: [u8; 6],
    scan_subnet: Option<(Ipv4Addr, u8)>,
    static_names: StaticNames,
    state: Arc<Mutex<Status>>,
    quit: Arc<AtomicBool>,
) {
    match Sockets::open(&iface) {
        Ok(socks) => {
            if let Err(e) = event_loop(socks, our_mac, scan_subnet, static_names, &state, &quit) {
                state
                    .lock()
                    .unwrap()
                    .log(format!("Inventory stopped: {e:#}"));
            }
        }
        Err(e) => {
            state
                .lock()
                .unwrap()
                .log(format!("Inventory disabled: {e:#}"));
        }
    }
}

fn event_loop(
    socks: Sockets,
    our_mac: [u8; 6],
    scan_subnet: Option<(Ipv4Addr, u8)>,
    static_names: StaticNames,
    state: &Arc<Mutex<Status>>,
    quit: &Arc<AtomicBool>,
) -> Result<()> {
    let our_ip = socks.our_ip;
    let icmp_id = std::process::id() as u16;
    let mut nodes: HashMap<[u8; 6], Node> = HashMap::new();
    let mut next_probe = Instant::now() + PROBE_INTERVAL;
    let mut next_sweep = Instant::now();
    let mut buf = [0u8; 2048];

    {
        let mut st = state.lock().unwrap();
        match our_ip {
            Some(ip) => st.log(format!("Inventory active on {ip} (ICMP+ARP probing)")),
            None => st.log(
                "Inventory active — interface has no IPv4 address, ARP-probe only".to_string(),
            ),
        }
        if let Some((net, prefix)) = scan_subnet {
            st.log(format!("Inventory: ARP-sweeping {net}/{prefix} every 60 s"));
        }
    }

    while !quit.load(Ordering::SeqCst) {
        let now = Instant::now();

        if let Some((net, prefix)) = scan_subnet {
            if now >= next_sweep {
                sweep(&socks, our_mac, our_ip, net, prefix);
                next_sweep = now + SWEEP_INTERVAL;
            }
        }

        if now >= next_probe {
            next_probe = now + PROBE_INTERVAL;
            for node in nodes.values_mut() {
                let Some(ip) = node.info.ip else { continue };
                finalize_tick(node);
                node.icmp_seq = node.icmp_seq.wrapping_add(1);
                let mut sent = false;
                if our_ip.is_some() {
                    sent |= socks.send_icmp_echo(ip, icmp_id, node.icmp_seq).is_ok();
                }
                sent |= socks.send_arp_request(our_mac, our_ip, ip).is_ok();
                if sent {
                    node.info.probes_sent += 1;
                    node.tick = Some(Tick {
                        sent_at: now,
                        icmp_rtt_us: None,
                        arp_rtt_us: None,
                    });
                }
                // Ask for the node's name via mDNS reverse lookup until
                // something answers (curated names make this a no-op).
                if node.info.name.is_none()
                    && node
                        .last_name_query
                        .is_none_or(|t| now.duration_since(t) >= NAME_QUERY_INTERVAL)
                    && socks.send_mdns_query(ip).is_ok()
                {
                    node.last_name_query = Some(now);
                }
            }
        }

        if socks.poll(50)? {
            // Drain the packet socket: passive learning + ARP replies.
            while let Some((n, pkttype)) = socks.recv_pkt(&mut buf)? {
                if pkttype == libc::PACKET_OUTGOING {
                    continue;
                }
                handle_frame(&buf[..n], our_mac, &static_names, &mut nodes, state);
            }
            // mDNS answers sent to us unicast instead of to the group.
            while let Some(n) = socks.recv_mdns(&mut buf)? {
                for (ip, name) in names::parse_dns_names(&buf[..n]) {
                    apply_name_by_ip(&mut nodes, ip, name, NameSource::Mdns, state);
                }
            }
            // Drain ICMP echo replies.
            while let Some((from, id, seq)) = socks.recv_icmp_reply(&mut buf)? {
                if id != icmp_id {
                    continue;
                }
                if let Some(node) = nodes.values_mut().find(|x| x.info.ip == Some(from)) {
                    if seq == node.icmp_seq {
                        if let Some(t) = &mut node.tick {
                            t.icmp_rtt_us
                                .get_or_insert(t.sent_at.elapsed().as_secs_f64() * 1e6);
                        }
                    }
                }
            }
        }

        publish(&nodes, state);
    }
    Ok(())
}

fn handle_frame(
    frame: &[u8],
    our_mac: [u8; 6],
    static_names: &StaticNames,
    nodes: &mut HashMap<[u8; 6], Node>,
    state: &Arc<Mutex<Status>>,
) {
    if frame.len() < 14 {
        return;
    }
    let mut src = [0u8; 6];
    src.copy_from_slice(&frame[6..12]);
    // Skip our own traffic, group addresses, and unset MACs.
    if src == our_mac || src[0] & 1 != 0 || src == [0u8; 6] {
        return;
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    // Extract everything of interest from the frame before touching the
    // node map, so name updates (which may target other nodes) can borrow
    // it freely afterwards.
    let mut ip = None;
    let mut arp_reply_to_us = false;
    let mut lldp_name = None;
    let mut dns_names = Vec::new();
    let mut dhcp_name = None;
    match ethertype {
        ETH_P_ARP if frame.len() >= 42 => {
            // sender MAC frame[22..28] (== src for real frames), sender IP [28..32]
            let spa = Ipv4Addr::new(frame[28], frame[29], frame[30], frame[31]);
            if !spa.is_unspecified() {
                ip = Some(spa);
            }
            arp_reply_to_us = u16::from_be_bytes([frame[20], frame[21]]) == 2
                && frame[0..6] == our_mac;
        }
        ETH_P_IPV4 if frame.len() >= 34 => {
            ip = Some(Ipv4Addr::new(frame[26], frame[27], frame[28], frame[29]));
            if let Some((src_port, dst_port, payload)) = udp_payload(frame) {
                if src_port == MDNS_PORT || dst_port == MDNS_PORT {
                    dns_names = names::parse_dns_names(payload);
                } else if dst_port == 67 {
                    dhcp_name = names::parse_dhcp_hostname(payload);
                }
            }
        }
        ETH_P_LLDP => {
            lldp_name = names::parse_lldp_sysname(&frame[14..]);
        }
        _ => {}
    }

    let now = Instant::now();
    let is_new = !nodes.contains_key(&src);
    if is_new && nodes.len() >= MAX_NODES {
        return;
    }
    let node = nodes.entry(src).or_insert_with(|| {
        let mut info = NodeInfo::new(src, now);
        if let Some(n) = static_names.by_mac.get(&src) {
            info.set_name(n.clone(), NameSource::Static);
        }
        Node {
            info,
            tick: None,
            icmp_seq: 0,
            last_name_query: None,
        }
    });
    node.info.last_seen = now;
    node.info.frames += 1;
    // First learned IP wins: nodes are keyed by MAC, and a multi-homed MAC
    // (e.g. a management interface with an alias) must not flap.
    if node.info.ip.is_none() && ip.is_some() {
        node.info.ip = ip;
        if let Some(n) = ip.and_then(|i| static_names.by_ip.get(&i)) {
            node.info.set_name(n.clone(), NameSource::Static);
        }
    }

    if let Some(name) = lldp_name {
        node.info.set_name(name, NameSource::Lldp);
    }

    if arp_reply_to_us {
        if let Some(t) = &mut node.tick {
            t.arp_rtt_us
                .get_or_insert(t.sent_at.elapsed().as_secs_f64() * 1e6);
        }
    }

    if is_new {
        let ip_str = ip.map(|i| format!(" ({i})")).unwrap_or_default();
        state.lock().unwrap().log(format!(
            "Node discovered: {}{ip_str}",
            fmt_mac(&src)
        ));
    }

    // These may name nodes other than the frame's sender.
    for (ip, name) in dns_names {
        apply_name_by_ip(nodes, ip, name, NameSource::Mdns, state);
    }
    if let Some((mac, name)) = dhcp_name {
        apply_name_by_mac(nodes, mac, name, NameSource::Dhcp, state);
    }
}

/// UDP (src port, dst port, payload) of an IPv4 frame, if it is UDP.
fn udp_payload(frame: &[u8]) -> Option<(u16, u16, &[u8])> {
    let ihl = ((*frame.get(14)? & 0x0F) as usize) * 4;
    if *frame.get(23)? != 17 {
        return None; // not UDP
    }
    let udp = frame.get(14 + ihl..)?;
    let src_port = u16::from_be_bytes([*udp.first()?, *udp.get(1)?]);
    let dst_port = u16::from_be_bytes([*udp.get(2)?, *udp.get(3)?]);
    Some((src_port, dst_port, udp.get(8..)?))
}

fn apply_name_by_ip(
    nodes: &mut HashMap<[u8; 6], Node>,
    ip: Ipv4Addr,
    name: String,
    source: NameSource,
    state: &Arc<Mutex<Status>>,
) {
    if let Some(node) = nodes.values_mut().find(|n| n.info.ip == Some(ip)) {
        log_named(&node.info, &name, source, state);
        node.info.set_name(name, source);
    }
}

fn apply_name_by_mac(
    nodes: &mut HashMap<[u8; 6], Node>,
    mac: [u8; 6],
    name: String,
    source: NameSource,
    state: &Arc<Mutex<Status>>,
) {
    if let Some(node) = nodes.get_mut(&mac) {
        log_named(&node.info, &name, source, state);
        node.info.set_name(name, source);
    }
}

fn log_named(info: &NodeInfo, name: &str, source: NameSource, state: &Arc<Mutex<Status>>) {
    let differs = match &info.name {
        Some((old, _)) => old != name,
        None => true,
    };
    let static_locked = matches!(info.name, Some((_, NameSource::Static)));
    if differs && !static_locked {
        state.lock().unwrap().log(format!(
            "Node {} identified as \"{name}\" ({})",
            fmt_mac(&info.mac),
            source.label()
        ));
    }
}

/// Fold the previous probe round into the node's stats.
fn finalize_tick(node: &mut Node) {
    let Some(t) = node.tick.take() else { return };
    let (rtt_us, method) = match (t.icmp_rtt_us, t.arp_rtt_us) {
        (Some(rtt), _) => (rtt, ProbeMethod::Icmp),
        (None, Some(rtt)) => (rtt, ProbeMethod::Arp),
        (None, None) => {
            node.info.probes_lost += 1;
            return;
        }
    };
    let info = &mut node.info;
    info.method = method;
    if let Some(avg) = info.rtt_avg_us {
        let jitter = (rtt_us - avg).abs();
        info.jitter_us = Some(info.jitter_us.map_or(jitter, |j| j * 0.8 + jitter * 0.2));
        info.rtt_avg_us = Some(avg * 0.8 + rtt_us * 0.2);
    } else {
        info.rtt_avg_us = Some(rtt_us);
    }
    info.rtt_last_us = Some(rtt_us);
}

fn sweep(socks: &Sockets, our_mac: [u8; 6], our_ip: Option<Ipv4Addr>, net: Ipv4Addr, prefix: u8) {
    let base = u32::from(net) & (u32::MAX << (32 - prefix));
    let hosts = 2u32.pow(32 - prefix as u32);
    for i in 1..hosts.saturating_sub(1) {
        let ip = Ipv4Addr::from(base + i);
        if Some(ip) != our_ip {
            let _ = socks.send_arp_request(our_mac, our_ip, ip);
        }
    }
}

fn publish(nodes: &HashMap<[u8; 6], Node>, state: &Arc<Mutex<Status>>) {
    let mut list: Vec<NodeInfo> = nodes.values().map(|n| n.info.clone()).collect();
    list.sort_by_key(|n| (n.ip.map(u32::from), n.mac));
    state.lock().unwrap().nodes = list;
}

pub fn fmt_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

struct Sockets {
    pkt_fd: RawFd,
    icmp_fd: Option<RawFd>,
    mdns_fd: Option<RawFd>,
    ifindex: i32,
    our_ip: Option<Ipv4Addr>,
}

impl Sockets {
    fn open(iface: &str) -> Result<Self> {
        let c_ifname = std::ffi::CString::new(iface)?;
        let ifindex = unsafe { libc::if_nametoindex(c_ifname.as_ptr()) };
        if ifindex == 0 {
            bail!("interface {iface} not found");
        }

        let pkt_fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                (ETH_P_ALL).to_be() as libc::c_int,
            )
        };
        if pkt_fd < 0 {
            return Err(io::Error::last_os_error()).context("opening inventory packet socket");
        }
        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = (ETH_P_ALL).to_be();
        sll.sll_ifindex = ifindex as i32;
        let rc = unsafe {
            libc::bind(
                pkt_fd,
                &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(pkt_fd) };
            return Err(e).context("binding inventory packet socket");
        }

        // Promiscuous mode: passive discovery should see multicast traffic
        // (mDNS, SSDP) that is not addressed to our NIC.
        let mut mreq: libc::packet_mreq = unsafe { std::mem::zeroed() };
        mreq.mr_ifindex = ifindex as i32;
        mreq.mr_type = libc::PACKET_MR_PROMISC as u16;
        unsafe {
            libc::setsockopt(
                pkt_fd,
                libc::SOL_PACKET,
                libc::PACKET_ADD_MEMBERSHIP,
                &mreq as *const libc::packet_mreq as *const libc::c_void,
                std::mem::size_of::<libc::packet_mreq>() as libc::socklen_t,
            )
        };

        let our_ip = if_ipv4(pkt_fd, iface);

        // Raw ICMP socket, pinned to the gPTP interface.
        let icmp_fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::IPPROTO_ICMP,
            )
        };
        let icmp_fd = if icmp_fd < 0 {
            None
        } else {
            let rc = unsafe {
                libc::setsockopt(
                    icmp_fd,
                    libc::SOL_SOCKET,
                    libc::SO_BINDTODEVICE,
                    c_ifname.as_ptr() as *const libc::c_void,
                    iface.len() as libc::socklen_t,
                )
            };
            if rc < 0 {
                unsafe { libc::close(icmp_fd) };
                None
            } else {
                Some(icmp_fd)
            }
        };

        // UDP socket for sending mDNS queries out of this interface.
        let mdns_fd = our_ip.and_then(|_| {
            let fd = unsafe {
                libc::socket(
                    libc::AF_INET,
                    libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                    0,
                )
            };
            if fd < 0 {
                return None;
            }
            let mreqn = libc::ip_mreqn {
                imr_multiaddr: libc::in_addr { s_addr: 0 },
                imr_address: libc::in_addr { s_addr: 0 },
                imr_ifindex: ifindex as i32,
            };
            let rc1 = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_BINDTODEVICE,
                    c_ifname.as_ptr() as *const libc::c_void,
                    iface.len() as libc::socklen_t,
                )
            };
            let rc2 = unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IP,
                    libc::IP_MULTICAST_IF,
                    &mreqn as *const libc::ip_mreqn as *const libc::c_void,
                    std::mem::size_of::<libc::ip_mreqn>() as libc::socklen_t,
                )
            };
            let ttl: libc::c_int = 255;
            let rc3 = unsafe {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_IP,
                    libc::IP_MULTICAST_TTL,
                    &ttl as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if rc1 < 0 || rc2 < 0 || rc3 < 0 {
                unsafe { libc::close(fd) };
                return None;
            }
            Some(fd)
        });

        Ok(Sockets {
            pkt_fd,
            icmp_fd,
            mdns_fd,
            ifindex: ifindex as i32,
            our_ip,
        })
    }

    fn send_mdns_query(&self, ip: Ipv4Addr) -> Result<()> {
        let Some(fd) = self.mdns_fd else {
            bail!("no mDNS socket");
        };
        let q = names::build_mdns_ptr_query(ip);
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = MDNS_PORT.to_be();
        sin.sin_addr.s_addr = u32::from(MDNS_ADDR).to_be();
        let rc = unsafe {
            libc::sendto(
                fd,
                q.as_ptr() as *const libc::c_void,
                q.len(),
                0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error()).context("sending mDNS query");
        }
        Ok(())
    }

    fn recv_mdns(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        let Some(fd) = self.mdns_fd else {
            return Ok(None);
        };
        let rc =
            unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e).context("receiving mDNS");
        }
        Ok(Some(rc as usize))
    }

    fn send_arp_request(
        &self,
        our_mac: [u8; 6],
        our_ip: Option<Ipv4Addr>,
        target: Ipv4Addr,
    ) -> Result<()> {
        let mut f = [0u8; 42];
        f[0..6].fill(0xFF);
        f[6..12].copy_from_slice(&our_mac);
        f[12..14].copy_from_slice(&ETH_P_ARP.to_be_bytes());
        f[14..16].copy_from_slice(&1u16.to_be_bytes()); // htype ethernet
        f[16..18].copy_from_slice(&ETH_P_IPV4.to_be_bytes());
        f[18] = 6;
        f[19] = 4;
        f[20..22].copy_from_slice(&1u16.to_be_bytes()); // op: request
        f[22..28].copy_from_slice(&our_mac);
        // Without a local IP this is an ARP probe (RFC 5227, spa 0.0.0.0).
        f[28..32].copy_from_slice(&our_ip.map_or([0; 4], |i| i.octets()));
        f[38..42].copy_from_slice(&target.octets());

        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = (ETH_P_ARP).to_be();
        sll.sll_ifindex = self.ifindex;
        sll.sll_halen = 6;
        sll.sll_addr[..6].fill(0xFF);
        let rc = unsafe {
            libc::sendto(
                self.pkt_fd,
                f.as_ptr() as *const libc::c_void,
                f.len(),
                0,
                &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error()).context("sending ARP request");
        }
        Ok(())
    }

    fn send_icmp_echo(&self, target: Ipv4Addr, id: u16, seq: u16) -> Result<()> {
        let Some(fd) = self.icmp_fd else {
            bail!("no ICMP socket");
        };
        let mut p = [0u8; 24];
        p[0] = 8; // echo request
        p[4..6].copy_from_slice(&id.to_be_bytes());
        p[6..8].copy_from_slice(&seq.to_be_bytes());
        p[8..24].copy_from_slice(b"gptp-gm-latency!");
        let sum = icmp_checksum(&p);
        p[2..4].copy_from_slice(&sum.to_be_bytes());

        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_addr.s_addr = u32::from(target).to_be();
        let rc = unsafe {
            libc::sendto(
                fd,
                p.as_ptr() as *const libc::c_void,
                p.len(),
                0,
                &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error()).context("sending ICMP echo");
        }
        Ok(())
    }

    fn recv_pkt(&self, buf: &mut [u8]) -> Result<Option<(usize, u8)>> {
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut addr_len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        let rc = unsafe {
            libc::recvfrom(
                self.pkt_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                &mut addr as *mut libc::sockaddr_ll as *mut libc::sockaddr,
                &mut addr_len,
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e).context("receiving on inventory socket");
        }
        Ok(Some((rc as usize, addr.sll_pkttype)))
    }

    /// Returns (source, id, seq) of a received ICMP echo reply.
    fn recv_icmp_reply(&self, buf: &mut [u8]) -> Result<Option<(Ipv4Addr, u16, u16)>> {
        let Some(fd) = self.icmp_fd else {
            return Ok(None);
        };
        loop {
            let rc = unsafe {
                libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
            };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(e).context("receiving ICMP");
            }
            let n = rc as usize;
            // Raw ICMP receive includes the IP header.
            if n < 20 {
                continue;
            }
            let ihl = ((buf[0] & 0x0F) as usize) * 4;
            if n < ihl + 8 || buf[ihl] != 0 {
                continue; // not an echo reply
            }
            let src = Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]);
            let id = u16::from_be_bytes([buf[ihl + 4], buf[ihl + 5]]);
            let seq = u16::from_be_bytes([buf[ihl + 6], buf[ihl + 7]]);
            return Ok(Some((src, id, seq)));
        }
    }

    fn poll(&self, timeout_ms: i32) -> Result<bool> {
        let mut pfds = [
            libc::pollfd {
                fd: self.pkt_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.icmp_fd.unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.mdns_fd.unwrap_or(-1),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e).context("polling inventory sockets");
        }
        Ok(rc > 0)
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.pkt_fd);
            if let Some(fd) = self.icmp_fd {
                libc::close(fd);
            }
            if let Some(fd) = self.mdns_fd {
                libc::close(fd);
            }
        }
    }
}

fn icmp_checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in data.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], 0])
        };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn if_ipv4(fd: RawFd, ifname: &str) -> Option<Ipv4Addr> {
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.as_bytes()) {
        *dst = *src as libc::c_char;
    }
    let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFADDR, &mut req) };
    if rc < 0 {
        return None;
    }
    let sin =
        unsafe { &*(&req.ifr_ifru.ifru_addr as *const libc::sockaddr as *const libc::sockaddr_in) };
    Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
}

/// Parse "a.b.c.d/len" with a sane sweep-size limit.
pub fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8)> {
    let (addr, prefix) = s
        .split_once('/')
        .with_context(|| format!("{s}: expected CIDR notation like 10.0.0.0/24"))?;
    let addr: Ipv4Addr = addr.parse().with_context(|| format!("bad address in {s}"))?;
    let prefix: u8 = prefix.parse().with_context(|| format!("bad prefix in {s}"))?;
    if !(22..=30).contains(&prefix) {
        bail!("sweep prefix must be /22../30 (got /{prefix})");
    }
    Ok((addr, prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_zero_type_echo() {
        // Verified against the standard ones'-complement algorithm.
        let mut p = [0u8; 8];
        p[0] = 8;
        let sum = icmp_checksum(&p);
        p[2..4].copy_from_slice(&sum.to_be_bytes());
        assert_eq!(icmp_checksum(&p), 0);
    }

    #[test]
    fn cidr_parsing() {
        assert_eq!(
            parse_cidr("10.0.0.0/24").unwrap(),
            (Ipv4Addr::new(10, 0, 0, 0), 24)
        );
        assert!(parse_cidr("10.0.0.0/8").is_err()); // too big to sweep
        assert!(parse_cidr("10.0.0.0").is_err());
        assert!(parse_cidr("nonsense/24").is_err());
    }
}
