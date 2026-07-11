//! Raw AF_PACKET socket bound to one interface for EtherType 0x88F7 (PTP).

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;

use anyhow::{bail, Context, Result};

use crate::ptp::{ETH_P_1588, PTP_MCAST};

pub struct RawSock {
    fd: RawFd,
    pub mac: [u8; 6],
}

impl RawSock {
    pub fn open(ifname: &str) -> Result<Self> {
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
                     the binary CAP_NET_RAW (sudo setcap cap_net_raw+ep <binary>)"
                );
            }
            return Err(e).context("opening AF_PACKET socket");
        }

        let mac = match read_mac(fd, ifname) {
            Ok(mac) => mac,
            Err(e) => {
                unsafe { libc::close(fd) };
                return Err(e);
            }
        };

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
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e).context("binding raw socket to interface");
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
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e).context("joining PTP multicast group");
        }

        Ok(RawSock { fd, mac })
    }

    /// Send a PTP payload framed to the gPTP multicast address.
    pub fn send(&self, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.extend_from_slice(&PTP_MCAST);
        frame.extend_from_slice(&self.mac);
        frame.extend_from_slice(&ETH_P_1588.to_be_bytes());
        frame.extend_from_slice(payload);
        let rc = unsafe {
            libc::send(
                self.fd,
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                0,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error()).context("sending PTP frame");
        }
        Ok(())
    }

    /// Receive one frame; returns the PTP payload with the Ethernet header
    /// stripped, or None if nothing is pending or the frame is our own.
    pub fn recv(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        let mut addr_len = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
        let rc = unsafe {
            libc::recvfrom(
                self.fd,
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
            return Err(e).context("receiving PTP frame");
        }
        let n = rc as usize;
        if addr.sll_pkttype == libc::PACKET_OUTGOING || n < 14 {
            return Ok(None);
        }
        buf.copy_within(14..n, 0);
        Ok(Some(n - 14))
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
        unsafe { libc::close(self.fd) };
    }
}

fn read_mac(fd: RawFd, ifname: &str) -> Result<[u8; 6]> {
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(ifname.as_bytes()) {
        *dst = *src as libc::c_char;
    }
    let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFHWADDR, &mut req) };
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
