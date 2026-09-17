use std::collections::HashSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub(crate) const INVALID_BIND_MESSAGE: &str =
    "http.bind must be auto:<port>, or a loopback, Tailscale, or private LAN address";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpBindTarget {
    Explicit(SocketAddr),
    Auto(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindAddressClass {
    Loopback,
    Tailnet,
    Lan,
}

impl fmt::Display for BindAddressClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Loopback => "loopback",
            Self::Tailnet => "tailnet",
            Self::Lan => "lan",
        })
    }
}

pub fn parse_http_bind(value: &str) -> Result<HttpBindTarget, String> {
    if let Some(port) = value.strip_prefix("auto:") {
        // `u16::parse` accepts a leading `+`; only plain digits are a port.
        let port = (port.bytes().all(|byte| byte.is_ascii_digit()) && !port.is_empty())
            .then(|| port.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port != 0)
            .ok_or_else(|| INVALID_BIND_MESSAGE.to_owned())?;
        return Ok(HttpBindTarget::Auto(port));
    }
    let address = value
        .parse::<SocketAddr>()
        .map_err(|_| INVALID_BIND_MESSAGE.to_owned())?;
    // An ephemeral port is meaningless in a stored configuration: nothing
    // could learn which port the daemon picked. Internal callers can still
    // pass port 0 to `HttpServer::bind` directly for tests.
    if address.port() == 0 || classify_bind_address(address.ip()).is_none() {
        return Err(INVALID_BIND_MESSAGE.to_owned());
    }
    Ok(HttpBindTarget::Explicit(address))
}

pub fn classify_bind_address(address: IpAddr) -> Option<BindAddressClass> {
    if address.is_loopback() {
        return Some(BindAddressClass::Loopback);
    }
    match address {
        IpAddr::V4(address) => {
            let address = u32::from(address);
            if address & 0xffc0_0000 == 0x6440_0000 {
                Some(BindAddressClass::Tailnet)
            } else if address & 0xff00_0000 == 0x0a00_0000
                || address & 0xfff0_0000 == 0xac10_0000
                || address & 0xffff_0000 == 0xc0a8_0000
            {
                Some(BindAddressClass::Lan)
            } else {
                None
            }
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            if segments[..3] == [0xfd7a, 0x115c, 0xa1e0] {
                Some(BindAddressClass::Tailnet)
            } else if segments[0] & 0xfe00 == 0xfc00 {
                Some(BindAddressClass::Lan)
            } else {
                None
            }
        }
    }
}

pub fn select_bind_addresses(
    addresses: impl IntoIterator<Item = IpAddr>,
    port: u16,
) -> Vec<SocketAddr> {
    let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut seen = HashSet::from([loopback]);
    let mut selected = vec![SocketAddr::new(loopback, port)];
    for address in addresses {
        if classify_bind_address(address).is_some() && seen.insert(address) {
            selected.push(SocketAddr::new(address, port));
        }
    }
    selected
}

pub fn discover_bind_addresses(port: u16) -> io::Result<Vec<SocketAddr>> {
    enumerate_local_addresses().map(|addresses| select_bind_addresses(addresses, port))
}

#[cfg(unix)]
fn enumerate_local_addresses() -> io::Result<Vec<IpAddr>> {
    let mut first = std::ptr::null_mut();
    // SAFETY: `first` is a valid output pointer and is released with `freeifaddrs` below.
    if unsafe { libc::getifaddrs(&mut first) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut addresses = Vec::new();
    let mut current = first;
    // SAFETY: `getifaddrs` returns a linked list that remains valid until `freeifaddrs`.
    unsafe {
        while let Some(interface) = current.as_ref() {
            let address = interface.ifa_addr;
            // An interface that is down keeps reporting its addresses, but
            // binding them fails with EADDRNOTAVAIL; skip them up front.
            let interface_up = u64::from(interface.ifa_flags) & libc::IFF_UP as u64 != 0;
            if !address.is_null() && interface_up {
                match i32::from((*address).sa_family) {
                    libc::AF_INET => {
                        let address = &*(address.cast::<libc::sockaddr_in>());
                        addresses.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            address.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let address = &*(address.cast::<libc::sockaddr_in6>());
                        addresses.push(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            current = interface.ifa_next;
        }
        libc::freeifaddrs(first);
    }
    Ok(addresses)
}

#[cfg(windows)]
fn enumerate_local_addresses() -> io::Result<Vec<IpAddr>> {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
    };

    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_SKIP_MULTICAST;
    let mut byte_length = 15_000u32;
    for _ in 0..3 {
        let word_length = (byte_length as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; word_length];
        let adapters = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: `buffer` is aligned and sized for `byte_length`; Windows initializes the
        // linked structures inside it and keeps every returned pointer within this call's buffer.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null(),
                adapters,
                &mut byte_length,
            )
        };
        if result == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if result != NO_ERROR {
            return Err(io::Error::from_raw_os_error(result as i32));
        }

        let mut addresses = Vec::new();
        let mut adapter = adapters;
        // SAFETY: the successful call populated linked adapter and unicast records inside
        // `buffer`, which remains alive for the duration of this traversal.
        unsafe {
            while let Some(current_adapter) = adapter.as_ref() {
                // An adapter whose OperStatus is not Up reports addresses that
                // cannot be bound; skip the whole adapter.
                if current_adapter.OperStatus
                    != windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp
                {
                    adapter = current_adapter.Next;
                    continue;
                }
                let mut unicast = current_adapter.FirstUnicastAddress;
                while let Some(current_unicast) = unicast.as_ref() {
                    let socket = current_unicast.Address;
                    if let Some(sockaddr) = socket.lpSockaddr.as_ref() {
                        match sockaddr.sa_family {
                            AF_INET
                                if socket.iSockaddrLength
                                    >= std::mem::size_of::<SOCKADDR_IN>() as i32 =>
                            {
                                let address = &*socket.lpSockaddr.cast::<SOCKADDR_IN>();
                                let bytes = address.sin_addr.S_un.S_un_b;
                                addresses.push(IpAddr::V4(Ipv4Addr::new(
                                    bytes.s_b1, bytes.s_b2, bytes.s_b3, bytes.s_b4,
                                )));
                            }
                            AF_INET6
                                if socket.iSockaddrLength
                                    >= std::mem::size_of::<SOCKADDR_IN6>() as i32 =>
                            {
                                let address = &*socket.lpSockaddr.cast::<SOCKADDR_IN6>();
                                addresses
                                    .push(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.u.Byte)));
                            }
                            _ => {}
                        }
                    }
                    unicast = current_unicast.Next;
                }
                adapter = current_adapter.Next;
            }
        }
        return Ok(addresses);
    }
    Err(io::Error::other(
        "local address enumeration buffer changed repeatedly",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_allowed_and_rejected_address_ranges() {
        for (class, addresses) in [
            (
                BindAddressClass::Loopback,
                &["127.0.0.1", "127.0.0.2", "::1"][..],
            ),
            (
                BindAddressClass::Tailnet,
                &[
                    "100.64.0.1",
                    "100.71.140.68",
                    "100.127.255.254",
                    "fd7a:115c:a1e0::1",
                ][..],
            ),
            (
                BindAddressClass::Lan,
                &[
                    "10.0.0.5",
                    "172.16.0.1",
                    "172.31.255.254",
                    "192.168.50.10",
                    "fd00::1",
                ][..],
            ),
        ] {
            for address in addresses {
                assert_eq!(classify_bind_address(address.parse().unwrap()), Some(class));
            }
        }
        for address in [
            "0.0.0.0",
            "::",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "169.254.1.1",
            "fe80::1",
            "8.8.8.8",
            "2001:4860:4860::8888",
            "224.0.0.1",
        ] {
            assert_eq!(
                classify_bind_address(address.parse().unwrap()),
                None,
                "{address}"
            );
        }
    }

    #[test]
    fn parses_auto_and_explicit_bind_targets() {
        assert_eq!(parse_http_bind("auto:7878"), Ok(HttpBindTarget::Auto(7878)));
        assert_eq!(
            parse_http_bind("192.168.50.10:7878"),
            Ok(HttpBindTarget::Explicit(
                "192.168.50.10:7878".parse().unwrap()
            ))
        );
        for bind in ["auto", "auto:0", "auto:abc", "auto:65536", "8.8.8.8:7878"] {
            let error = parse_http_bind(bind).unwrap_err();
            assert!(error.contains("http.bind"), "{bind}: {error}");
        }
    }

    #[test]
    fn selects_unique_allowed_addresses_and_always_includes_ipv4_loopback() {
        let selected = select_bind_addresses(
            [
                "8.8.8.8",
                "100.64.0.1",
                "169.254.1.1",
                "100.64.0.1",
                "192.168.50.10",
                "127.0.0.1",
            ]
            .map(|address| address.parse().unwrap()),
            7878,
        );
        assert_eq!(
            selected,
            [
                "127.0.0.1:7878".parse().unwrap(),
                "100.64.0.1:7878".parse().unwrap(),
                "192.168.50.10:7878".parse().unwrap(),
            ]
        );
    }
}
