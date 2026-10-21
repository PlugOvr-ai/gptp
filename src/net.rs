//! Raw AF_PACKET socket bound to one interface for EtherType 0x88F7 (PTP),
//! with hardware packet timestamping when the NIC supports it.
//!
//! Timestamp modes, best first:
//!   Hardware  — NIC timestamps frames in the MAC/PHY (SO_TIMESTAMPING raw
//!               hardware; stamps are in the PHC timebase, see phc.rs)
//!   Software  — kernel timestamps frames in the driver layer
//!               (CLOCK_REALTIME timebase)
//! If neither can be enabled the caller samples the clock around send/recv.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::phc::PhcClock;
use crate::ptp::{Timestamp, ETH_P_1588, PTP_MCAST};

const SO_TIMESTAMPING: libc::c_int = 37;
const SCM_TIMESTAMPING: libc::c_int = SO_TIMESTAMPING;
const SOF_TIMESTAMPING_TX_HARDWARE: u32 = 1 << 0;
const SOF_TIMESTAMPING_TX_SOFTWARE: u32 = 1 << 1;
const SOF_TIMESTAMPING_RX_HARDWARE: u32 = 1 << 2;
const SOF_TIMESTAMPING_RX_SOFTWARE: u32 = 1 << 3;
const SOF_TIMESTAMPING_SOFTWARE: u32 = 1 << 4;
const SOF_TIMESTAMPING_RAW_HARDWARE: u32 = 1 << 6;

const SIOCETHTOOL: libc::c_ulong = 0x8946;
/// Minimum Ethernet frame without FCS.
const MIN_FRAME: usize = 60;
const SIOCSHWTSTAMP: libc::c_ulong = 0x89b0;
const SIOCGHWTSTAMP: libc::c_ulong = 0x89b1;
const ETHTOOL_GET_TS_INFO: u32 = 0x41;

const HWTSTAMP_TX_ON: i32 = 1;
const HWTSTAMP_FILTER_ALL: i32 = 1;
const HWTSTAMP_FILTER_PTP_V2_L2_EVENT: i32 = 6;
const HWTSTAMP_FILTER_PTP_V2_EVENT: i32 = 12;
const HWTSTAMP_FILTER_NTP_ALL: i32 = 15;

#[repr(C)]
struct EthtoolTsInfo {
    cmd: u32,
    so_timestamping: u32,
    phc_index: i32,
    tx_types: u32,
    tx_reserved: [u32; 3],
    rx_filters: u32,
    rx_reserved: [u32; 3],
}

#[repr(C)]
struct HwtstampConfig {
    flags: i32,
    tx_type: i32,
    rx_filter: i32,
}

/// A packet timestamp in its native timebase (PHC if `hardware`, else
/// CLOCK_REALTIME). Convert before use.
#[derive(Clone, Copy, Debug)]
pub struct RawStamp {
    pub ts: Timestamp,
    pub hardware: bool,
}

pub enum TsMode {
    Hardware(PhcClock),
    Software,
    /// SO_TIMESTAMPING unavailable; caller samples the clock itself.
    None,
}

pub struct RawSock {
    fd: RawFd,
    /// Send-only socket without timestamping for general messages
    /// (Announce, Follow_Up, ...), as ptp4l does: on the timestamping socket
    /// they would request transmit stamps too, and many NICs take only one
    /// at a time — the next Sync's or Pdelay's stamp could be lost.
    gen_fd: RawFd,
    pub mac: [u8; 6],
    ts_mode: TsMode,
    ts_desc: String,
}

/// (PTP payload length, receive timestamp, sender MAC)
pub type Received = (usize, Option<RawStamp>, [u8; 6]);

impl RawSock {
    pub fn open(ifname: &str, force_software: bool) -> Result<Self> {
        let c_ifname = CString::new(ifname)?;
        let ifindex = unsafe { libc::if_nametoindex(c_ifname.as_ptr()) };
        if ifindex == 0 {
            bail!("interface {ifname} not found");
        }

        let proto = (ETH_P_1588).to_be() as libc::c_int;
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                proto,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EPERM) {
                bail!(
                    "opening raw socket: permission denied — run as root or grant \
                     the binary CAP_NET_RAW+CAP_NET_ADMIN \
                     (sudo setcap 'cap_net_raw,cap_net_admin+ep' <binary>)"
                );
            }
            return Err(e).context("opening AF_PACKET socket");
        }
        let close_on_err = |e: anyhow::Error| -> anyhow::Error {
            unsafe { libc::close(fd) };
            e
        };

        let mac = read_mac(fd, ifname).map_err(close_on_err)?;

        let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        sll.sll_family = libc::AF_PACKET as u16;
        sll.sll_protocol = (ETH_P_1588).to_be();
        sll.sll_ifindex = ifindex as i32;
        let rc = unsafe {
            libc::bind(
                fd,
                &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(close_on_err(
                anyhow::Error::from(io::Error::last_os_error())
                    .context("binding raw socket to interface"),
            ));
        }

        // Receive the PTP link-local multicast group.
        let mut mreq: libc::packet_mreq = unsafe { std::mem::zeroed() };
        mreq.mr_ifindex = ifindex as i32;
        mreq.mr_type = libc::PACKET_MR_MULTICAST as u16;
        mreq.mr_alen = 6;
        mreq.mr_address[..6].copy_from_slice(&PTP_MCAST);
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_PACKET,
                libc::PACKET_ADD_MEMBERSHIP,
                &mreq as *const libc::packet_mreq as *const libc::c_void,
                std::mem::size_of::<libc::packet_mreq>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(close_on_err(
                anyhow::Error::from(io::Error::last_os_error())
                    .context("joining PTP multicast group"),
            ));
        }

        let gen_fd = open_general(ifindex).map_err(close_on_err)?;
        let (ts_mode, ts_desc) = init_timestamping(fd, ifname, force_software);
        Ok(RawSock {
            fd,
            gen_fd,
            mac,
            ts_mode,
            ts_desc,
        })
    }

    pub fn is_hardware_ts(&self) -> bool {
        matches!(self.ts_mode, TsMode::Hardware(_))
    }

    pub fn phc(&self) -> Option<&PhcClock> {
        match &self.ts_mode {
            TsMode::Hardware(phc) => Some(phc),
            _ => None,
        }
    }

    pub fn ts_desc(&self) -> &str {
        &self.ts_desc
    }

    fn frame(&self, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.extend_from_slice(&PTP_MCAST);
        frame.extend_from_slice(&self.mac);
        frame.extend_from_slice(&ETH_P_1588.to_be_bytes());
        frame.extend_from_slice(payload);
        // Pad to the Ethernet minimum ourselves (a Sync is 58 bytes): some
        // drivers pad short frames by copying them, and the copy loses its
        // transmit timestamp request — every Sync then misses its stamp.
        // Receivers go by messageLength, so trailing zeros are harmless.
        frame.resize(frame.len().max(MIN_FRAME), 0);
        frame
    }

    fn raw_send(&self, frame: &[u8]) -> Result<()> {
        send_on(self.fd, frame)
    }

    /// Send a PTP payload without needing its transmit timestamp.
    pub fn send(&self, payload: &[u8]) -> Result<()> {
        send_on(self.gen_fd, &self.frame(payload))
    }

    /// Send a PTP payload and return its transmit timestamp from the socket
    /// error queue (matched by message type + sequence id). Returns None if
    /// no timestamp arrives within 20 ms.
    pub fn send_stamped(&self, payload: &[u8]) -> Result<Option<RawStamp>> {
        let want_type = payload[0] & 0x0F;
        let want_seq = [payload[30], payload[31]];
        self.drain_errqueue();
        self.raw_send(&self.frame(payload))?;
        if matches!(self.ts_mode, TsMode::None) {
            return Ok(None);
        }

        // Some drivers deliver transmit stamps late (ptp4l users raise
        // tx_timestamp_timeout for them); a late stamp is still exact.
        let deadline = Instant::now() + Duration::from_millis(20);
        let mut buf = [0u8; 512];
        loop {
            match self.recvmsg_ts(&mut buf, libc::MSG_ERRQUEUE) {
                Ok(Some((n, _pkttype, stamp))) => {
                    // The errqueue message carries the original frame.
                    if n >= 14 + 32 && buf[14] & 0x0F == want_type && buf[44..46] == want_seq {
                        if let Some(stamp) = stamp {
                            return Ok(Some(stamp));
                        }
                        return Ok(None); // matched but carried no usable stamp
                    }
                }
                Ok(None) | Err(_) => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    // Error-queue readiness is signalled via POLLERR.
                    let mut pfd = libc::pollfd {
                        fd: self.fd,
                        events: 0,
                        revents: 0,
                    };
                    unsafe { libc::poll(&mut pfd, 1, 1) };
                }
            }
        }
    }

    fn drain_errqueue(&self) {
        let mut buf = [0u8; 512];
        while let Ok(Some(_)) = self.recvmsg_ts(&mut buf, libc::MSG_ERRQUEUE) {}
    }

    /// Receive one frame; returns the PTP payload length (Ethernet header
    /// stripped), its receive timestamp and the sender's MAC, or None if
    /// nothing is pending or the frame is our own.
    pub fn recv(&self, buf: &mut [u8]) -> Result<Option<Received>> {
        match self.recvmsg_ts(buf, 0) {
            Ok(Some((n, pkttype, stamp))) => {
                if pkttype == libc::PACKET_OUTGOING || n < 14 {
                    return Ok(None);
                }
                let mut src = [0u8; 6];
                src.copy_from_slice(&buf[6..12]);
                buf.copy_within(14..n, 0);
                Ok(Some((n - 14, stamp, src)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e).context("receiving PTP frame"),
        }
    }

    /// recvmsg with SCM_TIMESTAMPING cmsg parsing. Returns
    /// (length, packet type, stamp); None on EAGAIN.
    fn recvmsg_ts(
        &self,
        buf: &mut [u8],
        flags: libc::c_int,
    ) -> io::Result<Option<(usize, u8, Option<RawStamp>)>> {
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut ctrl = [0u64; 64]; // u64 for cmsghdr alignment
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_name = &mut addr as *mut libc::sockaddr_ll as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_ll>() as u32;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = ctrl.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = std::mem::size_of_val(&ctrl) as _;

        let rc = unsafe { libc::recvmsg(self.fd, &mut msg, flags | libc::MSG_DONTWAIT) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e);
        }

        let mut stamp = None;
        let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg.is_null() {
            let c = unsafe { &*cmsg };
            if c.cmsg_level == libc::SOL_SOCKET && c.cmsg_type == SCM_TIMESTAMPING {
                // struct scm_timestamping { struct timespec ts[3]; }
                // ts[0] = software, ts[2] = raw hardware
                let data = unsafe { libc::CMSG_DATA(cmsg) } as *const libc::timespec;
                let ts = |i: isize| -> Timestamp {
                    let t = unsafe { *data.offset(i) };
                    Timestamp {
                        secs: t.tv_sec as u64,
                        nanos: t.tv_nsec as u32,
                    }
                };
                let hw = ts(2);
                let sw = ts(0);
                if hw.secs != 0 || hw.nanos != 0 {
                    stamp = Some(RawStamp {
                        ts: hw,
                        hardware: true,
                    });
                } else if sw.secs != 0 || sw.nanos != 0 {
                    stamp = Some(RawStamp {
                        ts: sw,
                        hardware: false,
                    });
                }
            }
            cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
        }
        Ok(Some((rc as usize, addr.sll_pkttype, stamp)))
    }

    /// Wait until the socket is readable or the timeout elapses.
    pub fn poll(&self, timeout_ms: i32) -> Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(e).context("polling socket");
        }
        Ok(rc > 0 && (pfd.revents & libc::POLLIN) != 0)
    }
}

impl Drop for RawSock {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
            libc::close(self.gen_fd);
        }
    }
}

fn send_on(fd: RawFd, frame: &[u8]) -> Result<()> {
    let rc = unsafe { libc::send(fd, frame.as_ptr() as *const libc::c_void, frame.len(), 0) };
    if rc < 0 {
        return Err(io::Error::last_os_error()).context("sending PTP frame");
    }
    Ok(())
}

/// A send-only AF_PACKET socket on the interface (protocol 0: receives
/// nothing), without timestamping.
fn open_general(ifindex: u32) -> Result<RawFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("opening AF_PACKET send socket");
    }
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_ifindex = ifindex as i32;
    let rc = unsafe {
        libc::bind(
            fd,
            &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e).context("binding send socket to interface");
    }
    Ok(fd)
}

/// Probe and enable the best available timestamping mode.
fn init_timestamping(fd: RawFd, ifname: &str, force_software: bool) -> (TsMode, String) {
    let hw_err = if force_software {
        "disabled by --force-sw-timestamps".to_string()
    } else {
        match try_enable_hardware(fd, ifname) {
            Ok(phc) => {
                let device = phc.device.clone();
                return (TsMode::Hardware(phc), format!("hardware ({device})"));
            }
            Err(e) => format!("{e:#}"),
        }
    };

    // Fall back to kernel software timestamps.
    let sw_flags =
        SOF_TIMESTAMPING_TX_SOFTWARE | SOF_TIMESTAMPING_RX_SOFTWARE | SOF_TIMESTAMPING_SOFTWARE;
    if set_so_timestamping(fd, sw_flags).is_ok() {
        (TsMode::Software, format!("software — {hw_err}"))
    } else {
        (TsMode::None, format!("clock sampling only — {hw_err}"))
    }
}

fn try_enable_hardware(fd: RawFd, ifname: &str) -> Result<PhcClock> {
    // 1. Does the NIC advertise hardware timestamping and a PHC?
    let mut info: EthtoolTsInfo = unsafe { std::mem::zeroed() };
    info.cmd = ETHTOOL_GET_TS_INFO;
    ifr_ioctl(
        fd,
        ifname,
        SIOCETHTOOL,
        &mut info as *mut _ as *mut libc::c_char,
    )
    .context("querying NIC timestamping capabilities")?;
    let needed =
        SOF_TIMESTAMPING_TX_HARDWARE | SOF_TIMESTAMPING_RX_HARDWARE | SOF_TIMESTAMPING_RAW_HARDWARE;
    if info.so_timestamping & needed != needed {
        bail!("NIC reports no hardware timestamping support");
    }
    if info.phc_index < 0 {
        bail!("NIC has no PTP hardware clock (PHC)");
    }

    // 2. Turn on timestamping in the NIC (needs CAP_NET_ADMIN) — unless it
    //    already stamps transmitted frames and received gPTP event frames,
    //    e.g. set up by a ptp4l on the same port: reconfiguring the NIC
    //    under another PTP stack could disturb it.
    let mut current = HwtstampConfig {
        flags: 0,
        tx_type: 0,
        rx_filter: 0,
    };
    let already = ifr_ioctl(
        fd,
        ifname,
        SIOCGHWTSTAMP,
        &mut current as *mut _ as *mut libc::c_char,
    )
    .is_ok()
        && current.tx_type == HWTSTAMP_TX_ON
        && [
            HWTSTAMP_FILTER_ALL,
            HWTSTAMP_FILTER_PTP_V2_L2_EVENT,
            HWTSTAMP_FILTER_PTP_V2_EVENT,
            HWTSTAMP_FILTER_NTP_ALL,
        ]
        .contains(&current.rx_filter);
    let mut last = if already {
        anyhow::anyhow!("ok")
    } else {
        anyhow::anyhow!("no rx filter accepted")
    };
    for rx_filter in [
        HWTSTAMP_FILTER_PTP_V2_L2_EVENT,
        HWTSTAMP_FILTER_PTP_V2_EVENT,
        HWTSTAMP_FILTER_ALL,
    ] {
        if already {
            break;
        }
        let mut cfg = HwtstampConfig {
            flags: 0,
            tx_type: HWTSTAMP_TX_ON,
            rx_filter,
        };
        match ifr_ioctl(
            fd,
            ifname,
            SIOCSHWTSTAMP,
            &mut cfg as *mut _ as *mut libc::c_char,
        ) {
            Ok(()) => {
                last = anyhow::anyhow!("ok");
                break;
            }
            Err(e) => {
                if e.raw_os_error() == Some(libc::EPERM) {
                    return Err(anyhow::Error::from(e)
                        .context("enabling NIC timestamping needs CAP_NET_ADMIN"));
                }
                last = anyhow::Error::from(e).context("enabling NIC timestamping");
            }
        }
    }
    if last.to_string() != "ok" {
        return Err(last);
    }

    // 3. Request hardware stamps on this socket and open the PHC.
    set_so_timestamping(
        fd,
        SOF_TIMESTAMPING_TX_HARDWARE | SOF_TIMESTAMPING_RX_HARDWARE | SOF_TIMESTAMPING_RAW_HARDWARE,
    )
    .context("enabling SO_TIMESTAMPING")?;
    PhcClock::open(info.phc_index)
}

/// The PTP hardware clock index of a NIC (`/dev/ptpN`), if it has one and
/// supports hardware timestamping. Needs no privileges.
pub fn phc_index(ifname: &str) -> Result<Option<i32>> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("opening control socket");
    }
    let mut info: EthtoolTsInfo = unsafe { std::mem::zeroed() };
    info.cmd = ETHTOOL_GET_TS_INFO;
    let res = ifr_ioctl(
        fd,
        ifname,
        SIOCETHTOOL,
        &mut info as *mut _ as *mut libc::c_char,
    );
    unsafe { libc::close(fd) };
    res.with_context(|| format!("querying timestamping capabilities of {ifname}"))?;
    let needed =
        SOF_TIMESTAMPING_TX_HARDWARE | SOF_TIMESTAMPING_RX_HARDWARE | SOF_TIMESTAMPING_RAW_HARDWARE;
    Ok((info.so_timestamping & needed == needed && info.phc_index >= 0).then_some(info.phc_index))
}

fn set_so_timestamping(fd: RawFd, flags: u32) -> io::Result<()> {
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_TIMESTAMPING,
            &flags as *const u32 as *const libc::c_void,
            std::mem::size_of::<u32>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Run a netdevice ioctl whose ifreq.ifr_data points at `data`.
fn ifr_ioctl(
    fd: RawFd,
    ifname: &str,
    request: libc::c_ulong,
    data: *mut libc::c_char,
) -> io::Result<()> {
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.as_bytes()) {
        *dst = *src as libc::c_char;
    }
    req.ifr_ifru.ifru_data = data;
    let rc = unsafe { libc::ioctl(fd, request as libc::Ioctl, &mut req) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_mac(fd: RawFd, ifname: &str) -> Result<[u8; 6]> {
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.as_bytes()) {
        *dst = *src as libc::c_char;
    }
    let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFHWADDR as libc::Ioctl, &mut req) };
    if rc < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("reading MAC address of {ifname}"));
    }
    let sa_data = unsafe { req.ifr_ifru.ifru_hwaddr.sa_data };
    let mut mac = [0u8; 6];
    for (dst, src) in mac.iter_mut().zip(sa_data.iter()) {
        *dst = *src as u8;
    }
    Ok(mac)
}
