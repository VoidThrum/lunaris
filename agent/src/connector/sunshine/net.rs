//! How the host is reached: on the same link, through something that looks
//! like a VPN, or unknown. It decides the size of the video packets asked
//! for (`rtsp::build_sdp`).
//!
//! Ported from moonlight-qt (GPLv3) `app/backend/nvcomputer.cpp`
//! `NvComputer::getActiveAddressReachability`, as `app/streaming/session.cpp`
//! uses it ("Use 1392 byte video packets by default"; `RI_LAN`: "treat it as
//! a local address"; `RI_VPN`: "cap at 1024 bytes ... Treat it as remote even
//! if the target address is in RFC 1918 address space"): the local address
//! of a socket connected to the host names the interface the route takes,
//! and that interface's flags, MTU and name are looked at.
//!
//! Of the reference's signs of a VPN these are here: an MTU under 1500
//! ("Treat MTUs under 1500 as likely VPNs"), a point-to-point link ("This
//! check detects OpenVPN on Unix-like OSes"), a name that starts with
//! "ZeroTier" or contains "VPN". Not here: Qt's interface type (Virtual,
//! Ppp), the Windows NDIS types and the OpenVPN TAP address prefix, which
//! need what only Qt or Windows tell. On a system where the interfaces
//! cannot be listed (anything but Unix here) the answer is `Unknown`.
//!
//! The reference connects a TCP socket; here a UDP socket is connected,
//! which sends nothing and gives the same local address (the route's).

use std::net::IpAddr;

/// moonlight-qt nvcomputer.h `NvComputer::ReachabilityType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    /// `RI_UNKNOWN`
    Unknown,
    /// `RI_LAN`: "This address is on-link".
    Lan,
    /// `RI_VPN`: "It looks like our route to this PC is over a VPN".
    Vpn,
}

/// One address of a network interface, as far as the decision needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InterfaceAddress {
    pub name: String,
    pub address: IpAddr,
    /// Bits of the network part.
    pub prefix: u32,
    pub up: bool,
    pub point_to_point: bool,
    /// 0 when it is not known.
    pub mtu: u32,
}

fn in_subnet(a: &IpAddr, b: &IpAddr, prefix: u32) -> bool {
    fn same(a: &[u8], b: &[u8], prefix: u32) -> bool {
        let prefix = (prefix as usize).min(a.len() * 8);
        let (whole, rest) = (prefix / 8, prefix % 8);
        if a[..whole] != b[..whole] {
            return false;
        }
        rest == 0 || (a[whole] ^ b[whole]) >> (8 - rest) == 0
    }
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => same(&a.octets(), &b.octets(), prefix),
        (IpAddr::V6(a), IpAddr::V6(b)) => same(&a.octets(), &b.octets(), prefix),
        _ => false,
    }
}

/// The decision of `getActiveAddressReachability` for a socket whose local
/// address is `local` and whose peer is `remote`.
pub(super) fn classify(interfaces: &[InterfaceAddress], local: &IpAddr, remote: &IpAddr) -> Reachability {
    // "Ensure the interface is up"; the first address that matches decides.
    let Some(nic) = interfaces.iter().find(|nic| nic.up && nic.address == *local) else {
        return Reachability::Unknown;
    };
    if nic.mtu != 0 && nic.mtu < 1500 {
        return Reachability::Vpn;
    }
    if nic.point_to_point {
        return Reachability::Vpn;
    }
    if nic.name.starts_with("ZeroTier") || nic.name.contains("VPN") {
        return Reachability::Vpn;
    }
    // "Didn't meet any of our VPN heuristics. Let's see if the peer address is on-link."
    if in_subnet(local, remote, nic.prefix) {
        return Reachability::Lan;
    }
    Reachability::Unknown
}

#[cfg(unix)]
fn interfaces() -> Vec<InterfaceAddress> {
    use std::ffi::CStr;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // SAFETY: the sockaddr pointers of getifaddrs are read only as the
    // family they declare, and only while the list is alive.
    unsafe fn address(sa: *const libc::sockaddr) -> Option<IpAddr> {
        if sa.is_null() {
            return None;
        }
        match (*sa).sa_family as i32 {
            libc::AF_INET => {
                let sin = &*(sa as *const libc::sockaddr_in);
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))))
            }
            libc::AF_INET6 => {
                let sin6 = &*(sa as *const libc::sockaddr_in6);
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }

    fn prefix_of(mask: Option<IpAddr>) -> u32 {
        match mask {
            Some(IpAddr::V4(m)) => u32::from(m).leading_ones(),
            Some(IpAddr::V6(m)) => u128::from(m).leading_ones(),
            None => 0,
        }
    }

    /// The interface's MTU (SIOCGIFMTU), 0 when it cannot be asked.
    fn mtu_of(name: &CStr) -> u32 {
        // SAFETY: an ifreq is plain data; the name is copied with room for
        // its terminating zero; the socket is closed again.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return 0;
            }
            let mut request: libc::ifreq = std::mem::zeroed();
            let bytes = name.to_bytes();
            let n = bytes.len().min(request.ifr_name.len() - 1);
            for (to, from) in request.ifr_name.iter_mut().zip(&bytes[..n]) {
                *to = *from as libc::c_char;
            }
            let ok = libc::ioctl(fd, libc::SIOCGIFMTU as _, &mut request) == 0;
            libc::close(fd);
            if ok && request.ifr_ifru.ifru_mtu > 0 {
                request.ifr_ifru.ifru_mtu as u32
            } else {
                0
            }
        }
    }

    let mut found = Vec::new();
    // SAFETY: the list is walked as getifaddrs(3) describes and freed once.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            let Some(addr) = address(entry.ifa_addr) else { continue };
            if entry.ifa_name.is_null() {
                continue;
            }
            let name = CStr::from_ptr(entry.ifa_name);
            found.push(InterfaceAddress {
                name: name.to_string_lossy().into_owned(),
                address: addr,
                prefix: prefix_of(address(entry.ifa_netmask)),
                up: entry.ifa_flags & libc::IFF_UP as libc::c_uint != 0,
                point_to_point: entry.ifa_flags & libc::IFF_POINTOPOINT as libc::c_uint != 0,
                mtu: mtu_of(name),
            });
        }
        libc::freeifaddrs(list);
    }
    found
}

#[cfg(not(unix))]
fn interfaces() -> Vec<InterfaceAddress> {
    Vec::new()
}

/// How `remote` is reached from this computer. Sends nothing.
pub fn reachability(remote: &IpAddr) -> Reachability {
    let bind: std::net::SocketAddr = match remote {
        IpAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
        IpAddr::V6(_) => "[::]:0".parse().unwrap(),
    };
    // Connecting a UDP socket only asks the system for the route.
    let local = std::net::UdpSocket::bind(bind)
        .and_then(|socket| {
            socket.connect((*remote, 9))?;
            socket.local_addr()
        })
        .map(|addr| addr.ip());
    match local {
        Ok(local) => classify(&interfaces(), &local, remote),
        Err(_) => Reachability::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(name: &str, address: &str, prefix: u32, mtu: u32, point_to_point: bool) -> InterfaceAddress {
        InterfaceAddress { name: name.into(), address: address.parse().unwrap(), prefix, up: true, point_to_point, mtu }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_route_decides_as_in_moonlight() {
        let all = vec![
            nic("enp5s0", "192.168.45.93", 24, 1500, false),
            // A WireGuard interface as on the first real path: MTU 1420, point to point.
            nic("Home", "172.22.0.4", 32, 1420, true),
            nic("tun2", "10.10.10.2", 24, 1500, true),
            nic("short", "10.20.0.2", 24, 1400, false),
            nic("ZeroTier One", "10.30.0.2", 24, 2800, false),
            nic("Work VPN", "10.40.0.2", 24, 1500, false),
            nic("enp5s0", "fd00::2", 64, 1500, false),
        ];
        // On the link: 1392-byte packets are right.
        assert_eq!(classify(&all, &ip("192.168.45.93"), &ip("192.168.45.7")), Reachability::Lan);
        assert_eq!(classify(&all, &ip("fd00::2"), &ip("fd00::9")), Reachability::Lan);
        // A private address behind a tunnel is not a local network.
        assert_eq!(classify(&all, &ip("172.22.0.4"), &ip("192.168.1.20")), Reachability::Vpn);
        assert_eq!(classify(&all, &ip("10.10.10.2"), &ip("10.10.10.1")), Reachability::Vpn);
        assert_eq!(classify(&all, &ip("10.20.0.2"), &ip("10.20.0.1")), Reachability::Vpn);
        assert_eq!(classify(&all, &ip("10.30.0.2"), &ip("10.30.0.1")), Reachability::Vpn);
        assert_eq!(classify(&all, &ip("10.40.0.2"), &ip("10.40.0.1")), Reachability::Vpn);
        // Through a router, or from an address no interface has: not known.
        assert_eq!(classify(&all, &ip("192.168.45.93"), &ip("192.168.1.20")), Reachability::Unknown);
        assert_eq!(classify(&all, &ip("192.168.99.1"), &ip("192.168.99.2")), Reachability::Unknown);
        assert_eq!(classify(&[], &ip("192.168.45.93"), &ip("192.168.45.7")), Reachability::Unknown);
        // An interface that is down does not count.
        let mut down = all.clone();
        down[0].up = false;
        assert_eq!(classify(&down, &ip("192.168.45.93"), &ip("192.168.45.7")), Reachability::Unknown);
    }

    #[test]
    fn subnets() {
        assert!(in_subnet(&ip("10.1.2.3"), &ip("10.1.2.200"), 24));
        assert!(!in_subnet(&ip("10.1.2.3"), &ip("10.1.3.3"), 24));
        assert!(in_subnet(&ip("10.1.2.3"), &ip("10.1.3.3"), 22));
        assert!(in_subnet(&ip("10.1.2.3"), &ip("99.1.3.3"), 0));
        assert!(!in_subnet(&ip("10.1.2.3"), &ip("10.1.2.4"), 32));
        assert!(!in_subnet(&ip("10.1.2.3"), &ip("fd00::1"), 0));
    }

    #[cfg(unix)]
    #[test]
    fn this_computer_reaches_itself_on_the_link() {
        // The loopback interface: up, MTU 65536, 127.0.0.1/8.
        assert_eq!(reachability(&ip("127.0.0.1")), Reachability::Lan);
    }
}
