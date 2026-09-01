// Project:   factbook
// File:      src/geoip/private.rs
// Purpose:   Addresses that cannot have a geolocation answer
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Which addresses the lookup engine refuses to look up.
//!
//! Private, loopback, link-local, multicast, documentation and reserved space
//! is never allocated to a location, so a database traversal for one of them
//! can only ever return nothing. On an internal feed these are most of the
//! traffic, which is why the check sits in front of the cache rather than
//! behind it.
//!
//! Almost every range is a stable `std` predicate. Only the two `std` still
//! keeps unstable -- the carrier-grade NAT range and the reserved range -- are
//! written out here, as the masks the RFCs define.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether `ip` is in space that carries no geolocation.
#[inline]
#[must_use]
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_private_v4(ip),
        IpAddr::V6(ip) => is_private_v6(ip),
    }
}

/// The IPv4 ranges that hold no public host.
#[inline]
fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_documentation()
        || is_shared_v4(ip)
        || is_reserved_v4(ip)
}

/// Carrier-grade NAT space, `100.64.0.0/10` per RFC 6598.
///
/// `Ipv4Addr::is_shared` covers this but is still unstable.
#[inline]
fn is_shared_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000
}

/// Space reserved for future use, `240.0.0.0/4` per RFC 1112.
///
/// `Ipv4Addr::is_reserved` covers this but is still unstable.
#[inline]
fn is_reserved_v4(ip: Ipv4Addr) -> bool {
    (ip.octets()[0] & 0b1111_0000) == 0b1111_0000
}

/// The IPv6 ranges that hold no public host.
#[inline]
fn is_private_v6(ip: Ipv6Addr) -> bool {
    // An IPv4-mapped address is an IPv4 address wearing a longer form, so it is
    // answered by the IPv4 rules -- ::ffff:127.0.0.1 is loopback, and none of
    // the IPv6 predicates would say so.
    if let Some(ip) = ip.to_ipv4_mapped() {
        return is_private_v4(ip);
    }

    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a literal the tests are asserting about.
    fn ip(literal: &str) -> IpAddr {
        literal.parse().unwrap()
    }

    #[test]
    fn rfc1918_space_is_private() {
        assert!(is_private(ip("10.0.0.1")));
        assert!(is_private(ip("172.16.5.4")));
        assert!(is_private(ip("172.31.255.255")));
        assert!(is_private(ip("192.168.1.1")));
    }

    #[test]
    fn loopback_link_local_and_unspecified_are_private() {
        assert!(is_private(ip("127.0.0.1")));
        assert!(is_private(ip("169.254.10.20")));
        assert!(is_private(ip("0.0.0.0")));
        assert!(is_private(ip("::1")));
        assert!(is_private(ip("::")));
        assert!(is_private(ip("fe80::1")));
    }

    #[test]
    fn carrier_grade_nat_space_is_private() {
        // 100.64.0.0/10 runs to 100.127.255.255, so the second octet is what
        // separates it from the public 100.x addresses either side.
        assert!(is_private(ip("100.64.0.1")));
        assert!(is_private(ip("100.100.50.1")));
        assert!(is_private(ip("100.127.255.255")));
        assert!(!is_private(ip("100.63.255.255")));
        assert!(!is_private(ip("100.128.0.0")));
    }

    #[test]
    fn reserved_space_is_private() {
        assert!(is_private(ip("240.0.0.1")));
        assert!(is_private(ip("255.255.255.254")));
        assert!(is_private(ip("255.255.255.255")));
        // The boundary probe sits below 224.0.0.0/4, not just below 240.0.0.0/4:
        // everything between the two is multicast, which is also not routable.
        assert!(!is_private(ip("223.255.255.255")));
    }

    #[test]
    fn documentation_space_is_private() {
        assert!(is_private(ip("192.0.2.1")));
        assert!(is_private(ip("198.51.100.1")));
        assert!(is_private(ip("203.0.113.1")));
    }

    #[test]
    fn multicast_is_private_in_both_families() {
        // The reference implementation this replaces missed both of these.
        assert!(is_private(ip("224.0.0.1")));
        assert!(is_private(ip("239.1.2.3")));
        assert!(is_private(ip("ff02::1")));
    }

    #[test]
    fn unique_local_ipv6_is_private() {
        assert!(is_private(ip("fc00::1")));
        assert!(is_private(ip("fd12:3456:789a::1")));
    }

    #[test]
    fn an_ipv4_mapped_address_answers_by_its_ipv4_rules() {
        assert!(is_private(ip("::ffff:127.0.0.1")));
        assert!(is_private(ip("::ffff:10.0.0.1")));
        assert!(!is_private(ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn routable_space_is_not_private() {
        assert!(!is_private(ip("8.8.8.8")));
        assert!(!is_private(ip("1.0.0.1")));
        assert!(!is_private(ip("2.125.160.216")));
        assert!(!is_private(ip("89.160.20.112")));
        assert!(!is_private(ip("172.15.0.1")));
        assert!(!is_private(ip("172.32.0.1")));
        assert!(!is_private(ip("2606:4700:4700::1111")));
    }
}
