use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::RawFd;
use std::time::Duration;

/// The kernel's smoothed round-trip time for a TCP socket. Costs one syscall and no traffic,
/// so it can be sampled every second without disturbing the session.
pub fn tcp_rtt(fd: RawFd) -> Option<Duration> {
    // SAFETY: `tcp_info` is plain old data, so all-zeroes is a valid value.
    let mut info: libc::tcp_info = unsafe { std::mem::zeroed() };
    let mut len = libc::socklen_t::try_from(size_of::<libc::tcp_info>()).ok()?;
    // SAFETY: `info` and `len` are valid for writes and `len` holds the buffer size. A stale
    // descriptor makes getsockopt fail with EBADF; it can't write out of bounds.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&raw mut info).cast::<libc::c_void>(),
            &mut len,
        )
    };
    (ret == 0 && info.tcpi_rtt > 0).then(|| Duration::from_micros(u64::from(info.tcpi_rtt)))
}

/// Every IP address on this machine's interfaces. `None` if they can't be read, which callers
/// treat as "unknown", never as "gone".
pub fn local_addresses() -> Option<Vec<IpAddr>> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: on success `list` holds a linked list that we free below.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return None;
    }
    let mut addresses = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: `node` belongs to the list from getifaddrs, alive until freeifaddrs.
        let entry = unsafe { &*node };
        // SAFETY: `ifa_addr` is null or a sockaddr whose family says what it really is.
        if let Some(ip) = unsafe { sockaddr_ip(entry.ifa_addr) } {
            addresses.push(ip);
        }
        node = entry.ifa_next;
    }
    // SAFETY: `list` came from getifaddrs and isn't used again.
    unsafe { libc::freeifaddrs(list) };
    Some(addresses)
}

/// Whether `ip` is still on one of this machine's interfaces. A connection whose local address
/// is gone (a VPN profile or Wi-Fi network was switched) can never work again.
pub fn address_assigned(ip: IpAddr) -> Option<bool> {
    local_addresses().map(|addresses| addresses.contains(&ip))
}

/// The addresses that can reach other machines, sorted. When this changes while reconnecting,
/// a new network (or VPN tunnel) has come up and it's worth trying again right away.
pub fn routable_addresses() -> Option<Vec<IpAddr>> {
    local_addresses().map(routable)
}

/// Leaves out loopback and link-local addresses, which come and go with containers and IPv6
/// housekeeping without saying anything about the route to the host.
fn routable(mut addresses: Vec<IpAddr>) -> Vec<IpAddr> {
    addresses.retain(|ip| match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local(),
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unicast_link_local(),
    });
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

/// # Safety
/// `addr` is null or points to a valid sockaddr whose family matches its actual type.
unsafe fn sockaddr_ip(addr: *const libc::sockaddr) -> Option<IpAddr> {
    if addr.is_null() {
        return None;
    }
    // SAFETY: per the contract, `addr` points to a sockaddr of the type its family names.
    unsafe {
        match i32::from((*addr).sa_family) {
            libc::AF_INET => {
                let sin = addr.cast::<libc::sockaddr_in>().read_unaligned();
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.s_addr,
                ))))
            }
            libc::AF_INET6 => {
                let sin6 = addr.cast::<libc::sockaddr_in6>().read_unaligned();
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_include_loopback() {
        let addresses = local_addresses().expect("getifaddrs");
        assert!(addresses.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(
            address_assigned(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            Some(true)
        );
        assert_eq!(
            address_assigned(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
            Some(false)
        );
    }

    #[test]
    fn routable_ignores_loopback_and_link_local() {
        let vpn: IpAddr = "10.88.88.2".parse().unwrap();
        let wifi: IpAddr = "172.30.0.54".parse().unwrap();
        let addresses = vec![
            wifi,
            "127.0.0.1".parse().unwrap(),
            "::1".parse().unwrap(),
            "169.254.3.4".parse().unwrap(),
            "fe80::1".parse().unwrap(),
            vpn,
            wifi,
        ];
        assert_eq!(routable(addresses), vec![vpn, wifi]);
    }
}
