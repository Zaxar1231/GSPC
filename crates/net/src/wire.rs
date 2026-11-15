//! Разбор и сборка пакетов: Ethernet, ARP, IPv4, ICMP-эхо, UDP, TCP.
//!
//! Только то, что нужно NAT: без фрагментации IP, без опций IP, без IPv6. Контрольные суммы
//! UDP/TCP от гостя не проверяются (кадр не покидал памяти хоста), свои считаются всегда.

use std::net::Ipv4Addr;

/// MAC-адрес.
pub type Mac = [u8; 6];

/// Широковещательный MAC.
pub const BROADCAST: Mac = [0xff; 6];

/// Тип кадра: IPv4.
pub const ETHERTYPE_IPV4: u16 = 0x0800;
/// Тип кадра: ARP.
pub const ETHERTYPE_ARP: u16 = 0x0806;

/// Протокол IP: ICMP.
pub const PROTO_ICMP: u8 = 1;
/// Протокол IP: TCP.
pub const PROTO_TCP: u8 = 6;
/// Протокол IP: UDP.
pub const PROTO_UDP: u8 = 17;

/// Флаги TCP.
pub mod flags {
    /// FIN.
    pub const FIN: u8 = 0x01;
    /// SYN.
    pub const SYN: u8 = 0x02;
    /// RST.
    pub const RST: u8 = 0x04;
    /// PSH.
    pub const PSH: u8 = 0x08;
    /// ACK.
    pub const ACK: u8 = 0x10;
}

/// Кадр Ethernet.
#[derive(Debug, Clone, Copy)]
pub struct Eth<'a> {
    /// Получатель.
    pub dst: Mac,
    /// Отправитель.
    pub src: Mac,
    /// Тип.
    pub ethertype: u16,
    /// Данные.
    pub payload: &'a [u8],
}

fn mac_at(b: &[u8], at: usize) -> Option<Mac> {
    b.get(at..at + 6)?.try_into().ok()
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn ip_at(b: &[u8], at: usize) -> Option<Ipv4Addr> {
    let o: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(Ipv4Addr::from(o))
}

/// Разбирает кадр Ethernet.
#[must_use]
pub fn parse_eth(frame: &[u8]) -> Option<Eth<'_>> {
    Some(Eth {
        dst: mac_at(frame, 0)?,
        src: mac_at(frame, 6)?,
        ethertype: u16_at(frame, 12)?,
        payload: frame.get(14..)?,
    })
}

/// Собирает кадр Ethernet.
#[must_use]
pub fn build_eth(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(14 + payload.len());
    v.extend_from_slice(&dst);
    v.extend_from_slice(&src);
    v.extend_from_slice(&ethertype.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Сумма для контрольной суммы Интернета (без свёртки).
fn sum(data: &[u8], mut acc: u32) -> u32 {
    let (pairs, rest) = data.as_chunks::<2>();
    for c in pairs {
        acc += u32::from(u16::from_be_bytes(*c));
    }
    if let [last] = rest {
        acc += u32::from(*last) << 8;
    }
    acc
}

fn fold(mut acc: u32) -> u16 {
    while acc > 0xffff {
        acc = (acc & 0xffff) + (acc >> 16);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "после свёртки влезает в 16 бит"
    )]
    let v = acc as u16;
    !v
}

/// Контрольная сумма Интернета.
#[must_use]
pub fn checksum(data: &[u8]) -> u16 {
    fold(sum(data, 0))
}

fn pseudo_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, segment: &[u8]) -> u16 {
    let mut acc = sum(&src.octets(), 0);
    acc = sum(&dst.octets(), acc);
    acc += u32::from(proto);
    #[allow(clippy::cast_possible_truncation, reason = "сегмент короче 64 КиБ")]
    let len = segment.len() as u16;
    acc += u32::from(len);
    fold(sum(segment, acc))
}

/// Пакет IPv4 (без фрагментов).
#[derive(Debug, Clone, Copy)]
pub struct Ipv4<'a> {
    /// Отправитель.
    pub src: Ipv4Addr,
    /// Получатель.
    pub dst: Ipv4Addr,
    /// Протокол.
    pub proto: u8,
    /// Данные.
    pub payload: &'a [u8],
}

/// Разбирает IPv4; фрагменты и неверные заголовки отбрасываются.
#[must_use]
pub fn parse_ipv4(p: &[u8]) -> Option<Ipv4<'_>> {
    let b0 = *p.first()?;
    let ihl = usize::from(b0 & 0x0f) * 4;
    if b0 >> 4 != 4 || ihl < 20 || p.len() < ihl {
        return None;
    }
    let total = usize::from(u16_at(p, 2)?);
    if total < ihl || total > p.len() {
        return None;
    }
    // MF или смещение фрагмента — не собираем.
    if u16_at(p, 6)? & 0x3fff != 0 {
        return None;
    }
    if checksum(p.get(..ihl)?) != 0 {
        return None;
    }
    Some(Ipv4 {
        src: ip_at(p, 12)?,
        dst: ip_at(p, 16)?,
        proto: *p.get(9)?,
        payload: p.get(ihl..total)?,
    })
}

/// Собирает IPv4 без опций.
#[must_use]
pub fn build_ipv4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20 + payload.len());
    #[allow(clippy::cast_possible_truncation, reason = "кадр не длиннее MTU")]
    let total = (20 + payload.len()) as u16;
    v.extend_from_slice(&[0x45, 0]);
    v.extend_from_slice(&total.to_be_bytes());
    // id = 0, DF, TTL 64.
    v.extend_from_slice(&[0, 0, 0x40, 0, 64, proto, 0, 0]);
    v.extend_from_slice(&src.octets());
    v.extend_from_slice(&dst.octets());
    let c = checksum(&v);
    v[10..12].copy_from_slice(&c.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// ARP-сообщение для Ethernet/IPv4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arp {
    /// 1 — запрос, 2 — ответ.
    pub op: u16,
    /// MAC отправителя.
    pub sha: Mac,
    /// IP отправителя.
    pub spa: Ipv4Addr,
    /// MAC цели.
    pub tha: Mac,
    /// IP цели.
    pub tpa: Ipv4Addr,
}

/// Разбирает ARP.
#[must_use]
pub fn parse_arp(p: &[u8]) -> Option<Arp> {
    if u16_at(p, 0)? != 1 || u16_at(p, 2)? != ETHERTYPE_IPV4 || *p.get(4)? != 6 || *p.get(5)? != 4 {
        return None;
    }
    Some(Arp {
        op: u16_at(p, 6)?,
        sha: mac_at(p, 8)?,
        spa: ip_at(p, 14)?,
        tha: mac_at(p, 18)?,
        tpa: ip_at(p, 24)?,
    })
}

/// Собирает ARP.
#[must_use]
pub fn build_arp(a: &Arp) -> Vec<u8> {
    let mut v = Vec::with_capacity(28);
    v.extend_from_slice(&[0, 1]);
    v.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    v.extend_from_slice(&[6, 4]);
    v.extend_from_slice(&a.op.to_be_bytes());
    v.extend_from_slice(&a.sha);
    v.extend_from_slice(&a.spa.octets());
    v.extend_from_slice(&a.tha);
    v.extend_from_slice(&a.tpa.octets());
    v
}

/// Датаграмма UDP.
#[derive(Debug, Clone, Copy)]
pub struct Udp<'a> {
    /// Порт отправителя.
    pub src_port: u16,
    /// Порт получателя.
    pub dst_port: u16,
    /// Данные.
    pub payload: &'a [u8],
}

/// Разбирает UDP.
#[must_use]
pub fn parse_udp(p: &[u8]) -> Option<Udp<'_>> {
    let len = usize::from(u16_at(p, 4)?);
    if len < 8 || len > p.len() {
        return None;
    }
    Some(Udp {
        src_port: u16_at(p, 0)?,
        dst_port: u16_at(p, 2)?,
        payload: p.get(8..len)?,
    })
}

/// Собирает UDP (с контрольной суммой).
#[must_use]
pub fn build_udp(src: Ipv4Addr, dst: Ipv4Addr, sp: u16, dp: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    #[allow(clippy::cast_possible_truncation, reason = "кадр не длиннее MTU")]
    let len = (8 + payload.len()) as u16;
    v.extend_from_slice(&sp.to_be_bytes());
    v.extend_from_slice(&dp.to_be_bytes());
    v.extend_from_slice(&len.to_be_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(payload);
    let mut c = pseudo_checksum(src, dst, PROTO_UDP, &v);
    if c == 0 {
        c = 0xffff;
    }
    v[6..8].copy_from_slice(&c.to_be_bytes());
    v
}

/// Сегмент TCP.
#[derive(Debug, Clone, Copy)]
pub struct Tcp<'a> {
    /// Порт отправителя.
    pub src_port: u16,
    /// Порт получателя.
    pub dst_port: u16,
    /// Номер последовательности.
    pub seq: u32,
    /// Номер подтверждения.
    pub ack: u32,
    /// Флаги ([`flags`]).
    pub flags: u8,
    /// Окно (до масштабирования).
    pub window: u16,
    /// Опция MSS из SYN.
    pub mss: Option<u16>,
    /// Опция масштаба окна из SYN.
    pub wscale: Option<u8>,
    /// Данные.
    pub payload: &'a [u8],
}

/// Разбирает TCP.
#[must_use]
pub fn parse_tcp(p: &[u8]) -> Option<Tcp<'_>> {
    let off = usize::from(*p.get(12)? >> 4) * 4;
    if off < 20 || off > p.len() {
        return None;
    }
    let (mut mss, mut wscale) = (None, None);
    let mut opts = p.get(20..off)?;
    while let Some((&kind, rest)) = opts.split_first() {
        match kind {
            0 => break,
            1 => opts = rest,
            _ => {
                let len = usize::from(*rest.first()?);
                if len < 2 || len - 1 > rest.len() {
                    break;
                }
                let body = rest.get(1..len - 1)?;
                match (kind, body) {
                    (2, [a, b]) => mss = Some(u16::from_be_bytes([*a, *b])),
                    (3, [s]) => wscale = Some((*s).min(14)),
                    _ => {}
                }
                opts = rest.get(len - 1..)?;
            }
        }
    }
    Some(Tcp {
        src_port: u16_at(p, 0)?,
        dst_port: u16_at(p, 2)?,
        seq: u32_at(p, 4)?,
        ack: u32_at(p, 8)?,
        flags: *p.get(13)?,
        window: u16_at(p, 14)?,
        mss,
        wscale,
        payload: p.get(off..)?,
    })
}

/// Что собрать в сегмент TCP.
#[derive(Debug, Clone, Copy)]
pub struct TcpOut<'a> {
    /// Порт отправителя.
    pub src_port: u16,
    /// Порт получателя.
    pub dst_port: u16,
    /// Номер последовательности.
    pub seq: u32,
    /// Номер подтверждения.
    pub ack: u32,
    /// Флаги.
    pub flags: u8,
    /// Окно.
    pub window: u16,
    /// Добавить опции MSS и масштаб окна 0 (для SYN-ACK).
    pub syn_options: Option<u16>,
    /// Данные.
    pub payload: &'a [u8],
}

/// Собирает TCP (с контрольной суммой).
#[must_use]
pub fn build_tcp(src: Ipv4Addr, dst: Ipv4Addr, s: &TcpOut<'_>) -> Vec<u8> {
    let opts_len = if s.syn_options.is_some() { 8 } else { 0 };
    let mut v = Vec::with_capacity(20 + opts_len + s.payload.len());
    v.extend_from_slice(&s.src_port.to_be_bytes());
    v.extend_from_slice(&s.dst_port.to_be_bytes());
    v.extend_from_slice(&s.seq.to_be_bytes());
    v.extend_from_slice(&s.ack.to_be_bytes());
    #[allow(clippy::cast_possible_truncation, reason = "длина заголовка ≤ 60")]
    v.push((((20 + opts_len) / 4) as u8) << 4);
    v.push(s.flags);
    v.extend_from_slice(&s.window.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    if let Some(mss) = s.syn_options {
        v.extend_from_slice(&[2, 4]);
        v.extend_from_slice(&mss.to_be_bytes());
        // NOP, масштаб окна 0.
        v.extend_from_slice(&[1, 3, 3, 0]);
    }
    v.extend_from_slice(s.payload);
    let c = pseudo_checksum(src, dst, PROTO_TCP, &v);
    v[16..18].copy_from_slice(&c.to_be_bytes());
    v
}

/// Ответ на эхо-запрос ICMP (`None` — это не эхо-запрос).
#[must_use]
pub fn icmp_echo_reply(p: &[u8]) -> Option<Vec<u8>> {
    if *p.first()? != 8 || *p.get(1)? != 0 || p.len() < 8 {
        return None;
    }
    let mut v = p.to_vec();
    v[0] = 0;
    v[2] = 0;
    v[3] = 0;
    let c = checksum(&v);
    v[2..4].copy_from_slice(&c.to_be_bytes());
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
    const B: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);

    #[test]
    fn checksum_known_vector() {
        // Пример из RFC 1071.
        assert_eq!(
            checksum(&[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7]),
            !0xddf2
        );
        // Нечётная длина.
        assert_eq!(checksum(&[0xff]), !0xff00);
    }

    #[test]
    fn ipv4_roundtrip_and_validation() {
        let p = build_ipv4(A, B, PROTO_UDP, b"hello");
        let ip = parse_ipv4(&p).expect("ipv4");
        assert_eq!(
            (ip.src, ip.dst, ip.proto, ip.payload),
            (A, B, PROTO_UDP, &b"hello"[..])
        );
        // Испорченная сумма заголовка.
        let mut bad = p.clone();
        bad[10] ^= 1;
        assert!(parse_ipv4(&bad).is_none());
        // Фрагмент отбрасывается.
        let mut frag = p;
        frag[6] = 0x20;
        let c = {
            frag[10] = 0;
            frag[11] = 0;
            checksum(&frag[..20])
        };
        frag[10..12].copy_from_slice(&c.to_be_bytes());
        assert!(parse_ipv4(&frag).is_none());
    }

    #[test]
    fn udp_roundtrip_checksum_verifies() {
        let u = build_udp(A, B, 1234, 53, b"dns?");
        let parsed = parse_udp(&u).expect("udp");
        assert_eq!(
            (parsed.src_port, parsed.dst_port, parsed.payload),
            (1234, 53, &b"dns?"[..])
        );
        assert_eq!(pseudo_checksum(A, B, PROTO_UDP, &u), 0);
    }

    #[test]
    fn tcp_roundtrip_with_syn_options() {
        let seg = TcpOut {
            src_port: 80,
            dst_port: 40000,
            seq: 0xdead_beef,
            ack: 7,
            flags: flags::SYN | flags::ACK,
            window: 65535,
            syn_options: Some(1460),
            payload: &[],
        };
        let b = build_tcp(B, A, &seg);
        assert_eq!(pseudo_checksum(B, A, PROTO_TCP, &b), 0);
        let t = parse_tcp(&b).expect("tcp");
        assert_eq!(
            (t.seq, t.ack, t.flags, t.window),
            (0xdead_beef, 7, 0x12, 65535)
        );
        assert_eq!((t.mss, t.wscale), (Some(1460), Some(0)));
        let data = TcpOut {
            syn_options: None,
            flags: flags::ACK | flags::PSH,
            payload: b"abc",
            ..seg
        };
        let b = build_tcp(B, A, &data);
        assert_eq!(pseudo_checksum(B, A, PROTO_TCP, &b), 0);
        assert_eq!(parse_tcp(&b).expect("tcp").payload, b"abc");
    }

    #[test]
    fn arp_roundtrip() {
        let a = Arp {
            op: 1,
            sha: [2, 0, 0, 0, 0, 1],
            spa: A,
            tha: [0; 6],
            tpa: Ipv4Addr::new(10, 0, 2, 2),
        };
        assert_eq!(parse_arp(&build_arp(&a)), Some(a));
        assert_eq!(parse_arp(&[0; 10]), None);
    }

    #[test]
    fn echo_reply_has_valid_checksum() {
        let mut req = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, b'x', b'y'];
        let c = checksum(&req);
        req[2..4].copy_from_slice(&c.to_be_bytes());
        let rep = icmp_echo_reply(&req).expect("echo");
        assert_eq!(rep[0], 0);
        assert_eq!(checksum(&rep), 0);
        assert_eq!(icmp_echo_reply(&rep), None);
    }

    #[test]
    fn truncated_input_is_rejected() {
        assert!(parse_eth(&[0; 5]).is_none());
        assert!(parse_ipv4(&[0x45]).is_none());
        assert!(parse_udp(&[0; 7]).is_none());
        assert!(parse_tcp(&[0; 19]).is_none());
    }
}
