//! IEEE 1588 PTP beside gPTP (`--ptp udp|l2`), for devices that do not
//! speak 802.1AS: the default profile — two-step Sync/Follow_Up, Announce,
//! end-to-end delay (Delay_Req/Delay_Resp) — served from the same clock
//! and timestamps as gPTP, on a second port of the same clock identity.
//!
//! Transports: UDP/IPv4 to 224.0.1.129, event port 319 and general port
//! 320 (Delay_Req sent unicast, as in hybrid mode, is answered unicast); or
//! Ethernet to 01:1B:19:00:00:00. Bridges forward these messages like any
//! other traffic, without correcting their residence time, so expect µs
//! rather than gPTP's ns behind a loaded switch.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::unix::io::RawFd;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::clock::{stamp_to_tai, tx_stamp_missed, TaiClock};
use crate::config::{Config, PtpTransport};
use crate::gm::{catch_up, log_to_duration};
use crate::inventory::fmt_mac;
use crate::net::{self, RawSock, RawStamp};
use crate::ptp::{self, AnnounceData, Msg, MsgBuilder, Timestamp};
use crate::state::{Ptp1588Client, Ptp1588Status, Status};

const PTP_PRIMARY: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
const EVENT_PORT: u16 = 319;
const GENERAL_PORT: u16 = 320;
/// logMinDelayReqInterval told to slaves in Delay_Resp (1 per second).
const LOG_MIN_DELAY_REQ: i8 = 0;
/// Clients not heard from for this long are dropped from the list.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Our port number for 1588; gPTP is port 1.
const PORT_NUMBER: u16 = 2;
/// Wait for a transmit timestamp, as for gPTP.
const TX_STAMP_WAIT: Duration = Duration::from_millis(20);

/// A UDP socket closed on drop.
struct Udp(RawFd);

impl Drop for Udp {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

enum Transport {
    L2(RawSock),
    Udp { event: Udp, general: Udp },
}

/// Where a received message came from, for the answer and the client list.
struct From {
    /// UDP: the sender, if it wrote to us unicast (answered unicast).
    unicast: Option<Ipv4Addr>,
    desc: String,
}

pub struct Port1588 {
    transport: Transport,
    builder: MsgBuilder,
    announce: AnnounceData,
    log_sync: i8,
    log_announce: i8,
    sync_interval: Duration,
    announce_interval: Duration,
    seq_sync: u16,
    seq_announce: u16,
    next_sync: Instant,
    next_announce: Instant,
    active: bool,
    /// Foreign 1588 grandmasters already reported.
    foreign: Vec<[u8; 8]>,
}

impl Port1588 {
    /// Open the 1588 port on the gPTP socket's interface. Returns the port
    /// and its description for the UI.
    pub fn open(cfg: &Config, transport: PtpTransport, gptp: &RawSock) -> Result<(Self, String)> {
        let (transport, desc) = match transport {
            PtpTransport::L2 => {
                let sock = RawSock::open_on(
                    cfg.iface(),
                    cfg.force_sw_timestamps,
                    false,
                    ptp::PTP1588_MCAST,
                )
                .context("opening the 1588 Ethernet socket")?;
                (
                    Transport::L2(sock),
                    "Ethernet (01:1B:19:00:00:00)".to_string(),
                )
            }
            PtpTransport::Udp => {
                let ifname = cfg.iface();
                let flags = if gptp.is_hardware_ts() {
                    // Hardware transmit stamps only (a software one would
                    // arrive first); receive stamps from software too, in
                    // case the NIC stamps only Ethernet PTP.
                    net::TS_HARDWARE_FLAGS | net::TS_RX_SOFTWARE_FLAGS
                } else {
                    net::TS_SOFTWARE_FLAGS
                };
                let event = udp_socket(ifname, EVENT_PORT)?;
                net::set_so_timestamping(event.0, flags)
                    .context("enabling timestamps on the PTP event socket")?;
                let general = udp_socket(ifname, GENERAL_PORT)?;
                let addr = ipv4_of(ifname);
                (
                    Transport::Udp { event, general },
                    format!(
                        "UDP/IPv4 {}:{EVENT_PORT}/{GENERAL_PORT} from {}",
                        PTP_PRIMARY,
                        addr.map_or("(no IPv4 address!)".into(), |a| a.to_string())
                    ),
                )
            }
        };
        let identity = ptp::clock_identity_from_mac(gptp.mac);
        let now = Instant::now();
        let log_sync = cfg.ptp_log_sync_interval;
        let log_announce = cfg.ptp_log_announce_interval;
        Ok((
            Port1588 {
                transport,
                builder: MsgBuilder {
                    clock_identity: identity,
                    port_number: PORT_NUMBER,
                    domain: cfg.ptp_domain,
                    sdo: ptp::SDO_1588,
                },
                announce: AnnounceData {
                    priority1: cfg.priority1,
                    clock_class: cfg.clock_class,
                    clock_accuracy: cfg.clock_accuracy,
                    variance: cfg.variance,
                    priority2: cfg.priority2,
                    gm_identity: identity,
                    steps_removed: 0,
                    time_source: cfg.time_source,
                },
                log_sync,
                log_announce,
                sync_interval: log_to_duration(log_sync),
                announce_interval: log_to_duration(log_announce),
                seq_sync: 0,
                seq_announce: 0,
                next_sync: now,
                next_announce: now,
                active: false,
                foreign: Vec::new(),
            },
            format!(
                "{desc}, domain {}, E2E, Sync every {} s",
                cfg.ptp_domain,
                log_to_duration(log_sync).as_secs_f64()
            ),
        ))
    }

    /// Serve (while gPTP announces) or stay silent.
    pub fn set_active(&mut self, active: bool, now: Instant) {
        if active && !self.active {
            self.next_sync = now;
            self.next_announce = now;
        }
        self.active = active;
    }

    pub fn fds(&self) -> Vec<RawFd> {
        match &self.transport {
            Transport::L2(s) => vec![s.fd()],
            Transport::Udp { event, general } => vec![event.0, general.0],
        }
    }

    /// The next send, while active.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.active.then(|| self.next_sync.min(self.next_announce))
    }

    /// Send Announce and Sync/Follow_Up when due.
    pub fn tick(
        &mut self,
        now: Instant,
        clock: &TaiClock,
        gptp: &RawSock,
        state: &Arc<Mutex<Status>>,
    ) {
        if !self.active {
            return;
        }
        if now >= self.next_announce {
            let a = &self.announce;
            let msg = self.builder.announce(
                self.seq_announce,
                self.log_announce,
                clock.utc_offset(),
                a.priority1,
                a.clock_class,
                a.clock_accuracy,
                a.variance,
                a.priority2,
                a.time_source,
            );
            self.seq_announce = self.seq_announce.wrapping_add(1);
            if self.send_general(&msg, None).is_ok() {
                with_status(state, |p| p.announce_tx += 1);
            }
            self.next_announce = catch_up(self.next_announce, self.announce_interval, now);
        }
        if now >= self.next_sync {
            let seq = self.seq_sync;
            self.seq_sync = self.seq_sync.wrapping_add(1);
            let sync = self.builder.sync(seq, self.log_sync);
            let fallback = clock.now();
            // As for gPTP: without a hardware stamp no Follow_Up (no guesses).
            let origin = match self.send_event_stamped(&sync) {
                Ok(raw) if tx_stamp_missed(gptp, &raw) => {
                    state.lock().unwrap().counters.tx_stamp_missed += 1;
                    with_status(state, |p| p.sync_tx += 1);
                    None
                }
                Ok(raw) => {
                    with_status(state, |p| p.sync_tx += 1);
                    Some(stamp_to_tai(raw, gptp, clock).unwrap_or(fallback))
                }
                Err(_) => None,
            };
            if let Some(o) = origin {
                let fup = self.builder.follow_up_1588(seq, o, self.log_sync);
                if self.send_general(&fup, None).is_ok() {
                    with_status(state, |p| p.follow_up_tx += 1);
                }
            }
            self.next_sync = catch_up(self.next_sync, self.sync_interval, now);
        }
    }

    /// Handle everything received: answer Delay_Req, report other masters.
    pub fn drain(
        &mut self,
        clock: &TaiClock,
        gptp: &RawSock,
        state: &Arc<Mutex<Status>>,
    ) -> Result<()> {
        let mut buf = [0u8; 1600];
        for i in 0..self.fds().len() {
            while let Some((n, stamp, from)) = self.recv(i, &mut buf)? {
                let payload = &buf[..n];
                if ptp::sdo(payload) != Some(ptp::SDO_1588)
                    || payload.get(4) != Some(&self.builder.domain)
                {
                    continue;
                }
                let t_rx = stamp_to_tai(stamp, gptp, clock).unwrap_or_else(|| clock.now());
                match ptp::parse(payload) {
                    Some(Msg::DelayReq {
                        seq,
                        source,
                        correction_ns,
                    }) => self.answer_delay_req(seq, source, correction_ns, t_rx, from, state),
                    Some(Msg::Announce { data, .. })
                        if data.gm_identity != self.announce.gm_identity
                            && !self.foreign.contains(&data.gm_identity) =>
                    {
                        self.foreign.push(data.gm_identity);
                        let ours_better = self.announce.bmca_key() < data.bmca_key();
                        let mut st = state.lock().unwrap();
                        st.log(format!(
                            "WARNING: another PTP (1588) master {} in domain {} from {} \
                             (prio1 {}, class {}) — {}",
                            ptp::fmt_clock_identity(&data.gm_identity),
                            self.builder.domain,
                            from.desc,
                            data.priority1,
                            data.clock_class,
                            if ours_better {
                                "we outrank it, but devices may follow either"
                            } else {
                                "it OUTRANKS us: 1588 devices will follow it"
                            }
                        ));
                        if let Some(p) = st.ptp1588.as_mut() {
                            p.foreign_masters = self.foreign.len();
                        }
                    }
                    _ => {}
                }
            }
        }
        // Forget silent clients.
        with_status(state, |p| {
            p.clients.retain(|c| c.last_seen.elapsed() < CLIENT_TIMEOUT)
        });
        Ok(())
    }

    fn answer_delay_req(
        &mut self,
        seq: u16,
        source: [u8; 10],
        correction_ns: i64,
        t4: Timestamp,
        from: From,
        state: &Arc<Mutex<Status>>,
    ) {
        with_status(state, |p| {
            p.delay_req_rx += 1;
            match p.clients.iter_mut().find(|c| c.port == source) {
                Some(c) => {
                    c.last_seen = Instant::now();
                    c.requests += 1;
                    c.addr = from.desc.clone();
                }
                None => p.clients.push(Ptp1588Client {
                    port: source,
                    addr: from.desc.clone(),
                    last_seen: Instant::now(),
                    requests: 1,
                }),
            }
        });
        if !self.active {
            return;
        }
        let resp = self
            .builder
            .delay_resp(seq, &source, t4, correction_ns, LOG_MIN_DELAY_REQ);
        if self.send_general(&resp, from.unicast).is_ok() {
            with_status(state, |p| p.delay_resp_tx += 1);
        }
    }

    fn send_general(&self, payload: &[u8], unicast: Option<Ipv4Addr>) -> Result<()> {
        match &self.transport {
            Transport::L2(s) => s.send(payload),
            Transport::Udp { general, .. } => sendto(
                general.0,
                payload,
                unicast.unwrap_or(PTP_PRIMARY),
                GENERAL_PORT,
            )
            .context("sending PTP general message"),
        }
    }

    fn send_event_stamped(&self, payload: &[u8]) -> Result<Option<RawStamp>> {
        match &self.transport {
            Transport::L2(s) => s.send_stamped(payload),
            Transport::Udp { event, .. } => udp_send_stamped(event.0, payload),
        }
    }

    /// Receive one message from socket `i` of `fds()`.
    fn recv(&self, i: usize, buf: &mut [u8]) -> Result<Option<(usize, Option<RawStamp>, From)>> {
        match &self.transport {
            Transport::L2(s) => Ok(s.recv(buf)?.map(|(n, stamp, mac)| {
                (
                    n,
                    stamp,
                    From {
                        unicast: None,
                        desc: fmt_mac(&mac),
                    },
                )
            })),
            Transport::Udp { event, general } => {
                let fd = if i == 0 { event.0 } else { general.0 };
                Ok(udp_recv(fd, buf, 0)?.map(|(n, stamp, src, dst)| {
                    (
                        n,
                        stamp,
                        From {
                            unicast: (!dst.is_some_and(|d| d.is_multicast())).then_some(src),
                            desc: src.to_string(),
                        },
                    )
                }))
            }
        }
    }
}

fn with_status(state: &Arc<Mutex<Status>>, f: impl FnOnce(&mut Ptp1588Status)) {
    if let Some(p) = state.lock().unwrap().ptp1588.as_mut() {
        f(p);
    }
}

fn setsockopt<T>(fd: RawFd, level: libc::c_int, name: libc::c_int, val: &T) -> io::Result<()> {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            val as *const T as *const libc::c_void,
            std::mem::size_of::<T>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sockaddr(addr: Ipv4Addr, port: u16) -> libc::sockaddr_in {
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_port = port.to_be();
    sa.sin_addr.s_addr = u32::from(addr).to_be();
    sa
}

/// A UDP socket on `ifname` bound to `port`, in the PTP multicast group.
fn udp_socket(ifname: &str, port: u16) -> Result<Udp> {
    let c_ifname = CString::new(ifname)?;
    let ifindex = unsafe { libc::if_nametoindex(c_ifname.as_ptr()) };
    if ifindex == 0 {
        bail!("interface {ifname} not found");
    }
    let fd = unsafe {
        libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("opening UDP socket");
    }
    let sock = Udp(fd);
    let one: libc::c_int = 1;
    let zero: libc::c_int = 0;
    setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, &one)?;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            c_ifname.as_ptr() as *const libc::c_void,
            c_ifname.as_bytes_with_nul().len() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error()).context("binding UDP socket to the interface");
    }
    let sa = sockaddr(Ipv4Addr::UNSPECIFIED, port);
    let rc = unsafe {
        libc::bind(
            fd,
            &sa as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EACCES) => anyhow::Error::from(e).context(format!(
                "binding UDP port {port} needs root or CAP_NET_BIND_SERVICE"
            )),
            Some(libc::EADDRINUSE) => anyhow::Error::from(e).context(format!(
                "UDP port {port} is taken — is ptp4l serving PTP over UDP here?"
            )),
            _ => anyhow::Error::from(e).context(format!("binding UDP port {port}")),
        });
    }
    let mreq = libc::ip_mreqn {
        imr_multiaddr: libc::in_addr {
            s_addr: u32::from(PTP_PRIMARY).to_be(),
        },
        imr_address: libc::in_addr { s_addr: 0 },
        imr_ifindex: ifindex as libc::c_int,
    };
    setsockopt(fd, libc::IPPROTO_IP, libc::IP_ADD_MEMBERSHIP, &mreq)
        .context("joining the PTP multicast group 224.0.1.129")?;
    setsockopt(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &mreq)
        .context("selecting the multicast interface")?;
    setsockopt(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_LOOP, &zero)?;
    setsockopt(fd, libc::IPPROTO_IP, libc::IP_MULTICAST_TTL, &one)?;
    setsockopt(fd, libc::IPPROTO_IP, libc::IP_PKTINFO, &one)?;
    Ok(sock)
}

/// The interface's IPv4 address, if it has one.
fn ipv4_of(ifname: &str) -> Option<Ipv4Addr> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.as_bytes()) {
        *dst = *src as libc::c_char;
    }
    let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFADDR as libc::Ioctl, &mut req) };
    unsafe { libc::close(fd) };
    if rc < 0 {
        return None;
    }
    let sa =
        unsafe { *(&req.ifr_ifru.ifru_addr as *const libc::sockaddr as *const libc::sockaddr_in) };
    Some(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr)))
}

fn sendto(fd: RawFd, payload: &[u8], addr: Ipv4Addr, port: u16) -> io::Result<()> {
    let sa = sockaddr(addr, port);
    let rc = unsafe {
        libc::sendto(
            fd,
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            0,
            &sa as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Send an event message to the multicast group and return its transmit
/// timestamp from the error queue (None if none within 20 ms).
fn udp_send_stamped(fd: RawFd, payload: &[u8]) -> Result<Option<RawStamp>> {
    let want = (payload[0] & 0x0F, [payload[30], payload[31]]);
    let mut buf = [0u8; 512];
    while let Ok(Some(_)) = udp_recv(fd, &mut buf, libc::MSG_ERRQUEUE) {}
    sendto(fd, payload, PTP_PRIMARY, EVENT_PORT).context("sending PTP event message")?;
    let deadline = Instant::now() + TX_STAMP_WAIT;
    loop {
        match udp_recv(fd, &mut buf, libc::MSG_ERRQUEUE) {
            Ok(Some((n, stamp, _, _))) => {
                // The looped-back packet may carry its Ethernet/IP/UDP
                // headers (42 bytes, 46 with a VLAN tag) or only the payload.
                let ours = [0usize, 42, 46]
                    .iter()
                    .any(|&o| n >= o + 32 && (buf[o] & 0x0F, [buf[o + 30], buf[o + 31]]) == want);
                if ours && stamp.is_some() {
                    return Ok(stamp);
                }
            }
            Ok(None) | Err(_) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                let mut pfd = libc::pollfd {
                    fd,
                    events: 0,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, 1) };
            }
        }
    }
}

/// (length, stamp, sender, destination address)
type UdpReceived = (usize, Option<RawStamp>, Ipv4Addr, Option<Ipv4Addr>);

/// recvmsg on a UDP socket.
fn udp_recv(fd: RawFd, buf: &mut [u8], flags: libc::c_int) -> io::Result<Option<UdpReceived>> {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    };
    let mut ctrl = [0u64; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_name = &mut addr as *mut libc::sockaddr_in as *mut libc::c_void;
    msg.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = std::mem::size_of_val(&ctrl) as _;
    let rc = unsafe { libc::recvmsg(fd, &mut msg, flags | libc::MSG_DONTWAIT) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(e);
    }
    let mut stamp = None;
    let mut dst = None;
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        stamp = stamp.or(unsafe { net::cmsg_stamp(cmsg) });
        let c = unsafe { &*cmsg };
        if c.cmsg_level == libc::IPPROTO_IP && c.cmsg_type == libc::IP_PKTINFO {
            let info = unsafe { *(libc::CMSG_DATA(cmsg) as *const libc::in_pktinfo) };
            dst = Some(Ipv4Addr::from(u32::from_be(info.ipi_addr.s_addr)));
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }
    let src = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    Ok(Some((rc as usize, stamp, src, dst)))
}
