//! Минимальный DHCP-сервер: одному гостю выдаётся фиксированный адрес.

use std::net::Ipv4Addr;

use crate::wire::Mac;

/// Тип сообщения: DISCOVER.
pub(crate) const DISCOVER: u8 = 1;
/// Тип сообщения: OFFER.
pub(crate) const OFFER: u8 = 2;
/// Тип сообщения: REQUEST.
pub(crate) const REQUEST: u8 = 3;
/// Тип сообщения: ACK.
pub(crate) const ACK: u8 = 5;

const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const FIXED_LEN: usize = 236;

/// Запрос клиента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    /// DISCOVER или REQUEST (прочие тоже возвращаются, NAT их игнорирует).
    pub(crate) kind: u8,
    /// Идентификатор транзакции.
    pub(crate) xid: u32,
    /// Флаги BOOTP.
    pub(crate) flags: u16,
    /// MAC клиента.
    pub(crate) chaddr: Mac,
}

/// Разбирает сообщение клиента (`op` = 1).
#[must_use]
pub(crate) fn parse(p: &[u8]) -> Option<Request> {
    if p.len() < FIXED_LEN + 4 || *p.first()? != 1 || p.get(FIXED_LEN..FIXED_LEN + 4)? != COOKIE {
        return None;
    }
    let xid = u32::from_be_bytes(p.get(4..8)?.try_into().ok()?);
    let flags = u16::from_be_bytes(p.get(10..12)?.try_into().ok()?);
    let chaddr: Mac = p.get(28..34)?.try_into().ok()?;
    let mut kind = 0;
    let mut opts = p.get(FIXED_LEN + 4..)?;
    while let Some((&code, rest)) = opts.split_first() {
        match code {
            0 => opts = rest,
            255 => break,
            _ => {
                let len = usize::from(*rest.first()?);
                let body = rest.get(1..1 + len)?;
                if code == 53 {
                    kind = *body.first()?;
                }
                opts = rest.get(1 + len..)?;
            }
        }
    }
    (kind != 0).then_some(Request {
        kind,
        xid,
        flags,
        chaddr,
    })
}

/// Параметры, которые выдаёт сервер.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lease {
    /// Адрес гостя.
    pub(crate) guest: Ipv4Addr,
    /// Адрес сервера и маршрутизатора.
    pub(crate) gateway: Ipv4Addr,
    /// Маска.
    pub(crate) netmask: Ipv4Addr,
    /// DNS-сервер.
    pub(crate) dns: Ipv4Addr,
}

/// Собирает ответ (`reply_kind` — [`OFFER`] или [`ACK`]).
#[must_use]
pub(crate) fn reply(req: &Request, reply_kind: u8, lease: &Lease) -> Vec<u8> {
    let mut v = vec![0u8; FIXED_LEN];
    v[0] = 2; // BOOTREPLY
    v[1] = 1; // Ethernet
    v[2] = 6;
    v[4..8].copy_from_slice(&req.xid.to_be_bytes());
    v[10..12].copy_from_slice(&req.flags.to_be_bytes());
    v[16..20].copy_from_slice(&lease.guest.octets()); // yiaddr
    v[20..24].copy_from_slice(&lease.gateway.octets()); // siaddr
    v[28..34].copy_from_slice(&req.chaddr);
    v.extend_from_slice(&COOKIE);
    let opt = |v: &mut Vec<u8>, code: u8, body: &[u8]| {
        v.push(code);
        #[allow(clippy::cast_possible_truncation, reason = "опции короче 256 байт")]
        v.push(body.len() as u8);
        v.extend_from_slice(body);
    };
    opt(&mut v, 53, &[reply_kind]);
    opt(&mut v, 54, &lease.gateway.octets());
    opt(&mut v, 51, &86_400u32.to_be_bytes());
    opt(&mut v, 1, &lease.netmask.octets());
    opt(&mut v, 3, &lease.gateway.octets());
    opt(&mut v, 6, &lease.dns.octets());
    opt(&mut v, 26, &1500u16.to_be_bytes());
    v.push(255);
    v
}

#[cfg(test)]
pub(crate) fn discover(xid: u32, mac: Mac) -> Vec<u8> {
    let mut v = vec![0u8; FIXED_LEN];
    v[0] = 1;
    v[1] = 1;
    v[2] = 6;
    v[4..8].copy_from_slice(&xid.to_be_bytes());
    v[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
    v[28..34].copy_from_slice(&mac);
    v.extend_from_slice(&COOKIE);
    v.extend_from_slice(&[53, 1, DISCOVER, 255]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: Mac = [2, 0, 0, 0, 0, 9];

    #[test]
    fn discover_gets_offer_with_lease() {
        let req = parse(&discover(0xabcd, MAC)).expect("discover");
        assert_eq!((req.kind, req.xid, req.chaddr), (DISCOVER, 0xabcd, MAC));
        let lease = Lease {
            guest: Ipv4Addr::new(10, 0, 2, 15),
            gateway: Ipv4Addr::new(10, 0, 2, 2),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            dns: Ipv4Addr::new(10, 0, 2, 3),
        };
        let r = reply(&req, OFFER, &lease);
        assert_eq!(&r[16..20], &[10, 0, 2, 15]);
        assert_eq!(&r[4..8], &0xabcdu32.to_be_bytes());
        let opts = &r[FIXED_LEN + 4..];
        assert_eq!(&opts[..3], &[53, 1, OFFER]);
        assert_eq!(*opts.last().expect("end"), 255);
        // Ответ — не запрос клиента.
        assert!(parse(&r).is_none());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse(&[1; 100]).is_none());
        let mut bad = discover(1, MAC);
        bad[FIXED_LEN] = 0;
        assert!(parse(&bad).is_none());
    }
}
