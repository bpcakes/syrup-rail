use std::{fmt, net::IpAddr};

/// The network address a cardholder used when they personally took part in a
/// payment, as the host established it.
///
/// The subscription workflows attach the address to customer-initiated token
/// sales and nowhere else, and a gateway adapter must not send one with a
/// merchant-initiated charge. It is request-scoped like the payment token, is
/// never persisted, and takes no part in idempotency, replay equality, or any
/// payment or entitlement decision Syrup Rail makes. It counts toward a
/// request's size limit, and a merchant account's own IP rules may act on it.
/// Provider text that repeats the address is not searched for it and is stored
/// as provider text, though card-number redaction may remove some or all of
/// it.
///
/// The host decides whose address this is and whether it may be disclosed to
/// the provider; it must never substitute the address of its own server or of a
/// merchant representative. Nothing here checks that the address is routable or
/// the cardholder's, so a host behind a proxy must take it from trusted ingress
/// rather than from the socket peer.
///
/// An IPv4-mapped IPv6 address is stored as its IPv4 form. A network address
/// is customer data, so `Debug` and `Display` never print it; read it with
/// [`CardholderIpAddress::get`].
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CardholderIpAddress(IpAddr);

impl CardholderIpAddress {
    pub const fn new(address: IpAddr) -> Self {
        Self(address.to_canonical())
    }

    pub const fn get(self) -> IpAddr {
        self.0
    }
}

impl From<IpAddr> for CardholderIpAddress {
    fn from(address: IpAddr) -> Self {
        Self::new(address)
    }
}

impl fmt::Debug for CardholderIpAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CardholderIpAddress([redacted])")
    }
}

impl fmt::Display for CardholderIpAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn mapped_ipv4_is_stored_as_ipv4_and_formatting_is_value_free() {
        let v4 = Ipv4Addr::new(203, 0, 113, 7);
        let mapped = CardholderIpAddress::new(IpAddr::V6(v4.to_ipv6_mapped()));
        assert_eq!(mapped.get(), IpAddr::V4(v4));
        assert_eq!(mapped, CardholderIpAddress::from(IpAddr::V4(v4)));

        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x17));
        assert_eq!(CardholderIpAddress::new(v6).get(), v6);

        for address in [mapped, CardholderIpAddress::new(v6)] {
            let rendered = format!("{address:?} {address}");
            assert!(!rendered.contains("203"), "{rendered}");
            assert!(!rendered.contains("2001"), "{rendered}");
            assert!(rendered.contains("[redacted]"));
        }
    }
}
