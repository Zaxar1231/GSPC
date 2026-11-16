//! Сквозные тесты NAT на loopback: «гость» собирает кадры, «сервер» — обычные сокеты.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::Duration;

use super::*;
use wire::{build_eth, build_ipv4, flags, parse_eth, parse_ipv4, parse_tcp, parse_udp};

const GUEST_MAC: Mac = [2, 0, 0, 0, 0, 1];
const GUEST_PORT: u16 = 40_000;

fn ip_frame(proto: u8, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let ip = build_ipv4(GUEST_IP, dst, proto, payload);
    build_eth(GATEWAY_MAC, GUEST_MAC, ETHERTYPE_IPV4, &ip)
}

fn tcp_frame(dst: Ipv4Addr, dport: u16, seq: u32, ack: u32, fl: u8, payload: &[u8]) -> Vec<u8> {
    let seg = TcpOut {
        src_port: GUEST_PORT,
        dst_port: dport,
        seq,
        ack,
        flags: fl,
        window: 65_535,
        syn_options: None,
        payload,
    };
    ip_frame(PROTO_TCP, dst, &wire::build_tcp(GUEST_IP, dst, &seg))
}

/// Принятый от NAT сегмент TCP.
#[derive(Debug, Clone)]
struct Seg {
    seq: u32,
    ack: u32,
    flags: u8,
    payload: Vec<u8>,
}

fn as_tcp(frame: &[u8]) -> Option<Seg> {
    let eth = parse_eth(frame)?;
    let ip = parse_ipv4(eth.payload)?;
    if ip.proto != PROTO_TCP {
        return None;
    }
    let t = parse_tcp(ip.payload)?;
    Some(Seg {
        seq: t.seq,
        ack: t.ack,
        flags: t.flags,
        payload: t.payload.to_vec(),
    })
}

/// Опрашивает NAT, пока не придёт сегмент TCP, подходящий под `pred` (прочие кадры пропускаются).
fn wait_seg(nat: &mut Nat, pred: impl Fn(&Seg) -> bool) -> Option<Seg> {
    for _ in 0..3000 {
        nat.poll(Instant::now());
        while let Some(f) = nat.pop_frame() {
            if let Some(s) = as_tcp(&f)
                && pred(&s)
            {
                return Some(s);
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}

fn nat() -> Nat {
    Nat::new(NatConfig::default())
}

fn listener() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = l.local_addr().expect("addr").port();
    (l, port)
}

/// Устанавливает соединение с `GATEWAY_IP:port`; возвращает сокет сервера и номера (наш isn).
fn connect(nat: &mut Nat, l: &TcpListener, port: u16) -> (TcpStream, u32) {
    nat.guest_frame(
        &tcp_frame(GATEWAY_IP, port, 1000, 0, flags::SYN, &[]),
        Instant::now(),
    );
    let synack = wait_seg(nat, |s| s.flags == flags::SYN | flags::ACK).expect("SYN-ACK");
    assert_eq!(synack.ack, 1001);
    let isn = synack.seq;
    nat.guest_frame(
        &tcp_frame(GATEWAY_IP, port, 1001, isn.wrapping_add(1), flags::ACK, &[]),
        Instant::now(),
    );
    let (srv, _) = l.accept().expect("accept");
    srv.set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    (srv, isn)
}

#[test]
fn arp_for_gateway_is_answered() {
    let mut n = nat();
    let req = wire::build_arp(&wire::Arp {
        op: 1,
        sha: GUEST_MAC,
        spa: GUEST_IP,
        tha: [0; 6],
        tpa: GATEWAY_IP,
    });
    n.guest_frame(
        &build_eth(BROADCAST, GUEST_MAC, ETHERTYPE_ARP, &req),
        Instant::now(),
    );
    let f = n.pop_frame().expect("ответ ARP");
    let eth = parse_eth(&f).expect("eth");
    assert_eq!(
        (eth.dst, eth.src, eth.ethertype),
        (GUEST_MAC, GATEWAY_MAC, ETHERTYPE_ARP)
    );
    let a = wire::parse_arp(eth.payload).expect("arp");
    assert_eq!(
        (a.op, a.sha, a.spa, a.tpa),
        (2, GATEWAY_MAC, GATEWAY_IP, GUEST_IP)
    );
    // Про самого себя гостю не отвечаем.
    let own = wire::build_arp(&wire::Arp {
        op: 1,
        sha: GUEST_MAC,
        spa: GUEST_IP,
        tha: [0; 6],
        tpa: GUEST_IP,
    });
    n.guest_frame(
        &build_eth(BROADCAST, GUEST_MAC, ETHERTYPE_ARP, &own),
        Instant::now(),
    );
    assert!(n.pop_frame().is_none());
}

#[test]
fn dhcp_discover_and_request_get_lease() {
    let mut n = nat();
    for (kind, want) in [(dhcp::DISCOVER, dhcp::OFFER), (dhcp::REQUEST, dhcp::ACK)] {
        let mut msg = dhcp::discover(7, GUEST_MAC);
        let at = msg.len() - 2;
        msg[at] = kind;
        let udp = wire::build_udp(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &msg);
        let ip = build_ipv4(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, PROTO_UDP, &udp);
        n.guest_frame(
            &build_eth(BROADCAST, GUEST_MAC, ETHERTYPE_IPV4, &ip),
            Instant::now(),
        );
        let f = n.pop_frame().expect("ответ DHCP");
        let eth = parse_eth(&f).expect("eth");
        assert_eq!(eth.dst, GUEST_MAC);
        let ip = parse_ipv4(eth.payload).expect("ip");
        assert_eq!(ip.src, GATEWAY_IP);
        let u = parse_udp(ip.payload).expect("udp");
        assert_eq!((u.src_port, u.dst_port), (67, 68));
        assert_eq!(&u.payload[16..20], &GUEST_IP.octets(), "yiaddr");
        assert_eq!(&u.payload[240..243], &[53, 1, want]);
    }
}

#[test]
fn udp_reaches_host_through_gateway_alias_and_back() {
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
    server
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let port = server.local_addr().expect("addr").port();
    let mut n = nat();
    let u = wire::build_udp(GUEST_IP, GATEWAY_IP, 5000, port, b"ping");
    n.guest_frame(&ip_frame(PROTO_UDP, GATEWAY_IP, &u), Instant::now());
    let mut buf = [0u8; 64];
    let (len, from) = server.recv_from(&mut buf).expect("датаграмма");
    assert_eq!(&buf[..len], b"ping");
    server.send_to(b"pong", from).expect("send");
    let mut got = None;
    for _ in 0..3000 {
        n.poll(Instant::now());
        if let Some(f) = n.pop_frame() {
            got = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let f = got.expect("ответ");
    let eth = parse_eth(&f).expect("eth");
    let ip = parse_ipv4(eth.payload).expect("ip");
    let u = parse_udp(ip.payload).expect("udp");
    assert_eq!((ip.src, ip.dst), (GATEWAY_IP, GUEST_IP));
    assert_eq!(
        (u.src_port, u.dst_port, u.payload),
        (port, 5000, &b"pong"[..])
    );
}

#[test]
fn dns_queries_go_to_upstream() {
    let upstream = UdpSocket::bind("127.0.0.1:0").expect("bind");
    upstream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let mut n = Nat::new(NatConfig {
        upstream_dns: upstream.local_addr().expect("addr"),
        ..NatConfig::default()
    });
    let u = wire::build_udp(GUEST_IP, DNS_IP, 6000, 53, b"query");
    n.guest_frame(&ip_frame(PROTO_UDP, DNS_IP, &u), Instant::now());
    let mut buf = [0u8; 64];
    let (len, from) = upstream.recv_from(&mut buf).expect("запрос");
    assert_eq!(&buf[..len], b"query");
    upstream.send_to(b"answer", from).expect("send");
    for _ in 0..3000 {
        n.poll(Instant::now());
        if let Some(f) = n.pop_frame() {
            let eth = parse_eth(&f).expect("eth");
            let ip = parse_ipv4(eth.payload).expect("ip");
            let u = parse_udp(ip.payload).expect("udp");
            assert_eq!(
                (ip.src, u.src_port, u.payload),
                (DNS_IP, 53, &b"answer"[..])
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    unreachable!("ответ DNS не пришёл");
}

#[test]
fn tcp_session_both_directions_and_close() {
    let (l, port) = listener();
    let mut n = nat();
    let (mut srv, isn) = connect(&mut n, &l, port);

    // Гость → сервер.
    n.guest_frame(
        &tcp_frame(
            GATEWAY_IP,
            port,
            1001,
            isn + 1,
            flags::ACK | flags::PSH,
            b"hello",
        ),
        Instant::now(),
    );
    let ack = wait_seg(&mut n, |s| s.flags == flags::ACK && s.payload.is_empty()).expect("ACK");
    assert_eq!(ack.ack, 1006);
    let mut buf = [0u8; 16];
    let got = srv.read(&mut buf).expect("read");
    assert_eq!(&buf[..got], b"hello");

    // Повтор того же сегмента: подтверждается, но второй раз не пересылается.
    n.guest_frame(
        &tcp_frame(
            GATEWAY_IP,
            port,
            1001,
            isn + 1,
            flags::ACK | flags::PSH,
            b"hello",
        ),
        Instant::now(),
    );
    let dup = wait_seg(&mut n, |s| s.flags == flags::ACK && s.payload.is_empty()).expect("ACK");
    assert_eq!(dup.ack, 1006);
    srv.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("timeout");
    assert!(
        srv.read(&mut buf).is_err(),
        "повтор не должен дойти до сервера"
    );

    // Сервер → гость.
    srv.write_all(b"world").expect("write");
    let data = wait_seg(&mut n, |s| !s.payload.is_empty()).expect("данные");
    assert_eq!(
        (data.payload.as_slice(), data.seq),
        (&b"world"[..], isn + 1)
    );
    n.guest_frame(
        &tcp_frame(GATEWAY_IP, port, 1006, isn + 6, flags::ACK, &[]),
        Instant::now(),
    );

    // Сервер закрывает: FIN гостю; гость подтверждает и закрывает со своей стороны.
    drop(srv);
    let fin = wait_seg(&mut n, |s| s.flags & flags::FIN != 0).expect("FIN");
    assert_eq!(fin.seq, isn + 6);
    n.guest_frame(
        &tcp_frame(
            GATEWAY_IP,
            port,
            1006,
            isn + 7,
            flags::ACK | flags::FIN,
            &[],
        ),
        Instant::now(),
    );
    let last = wait_seg(&mut n, |s| s.flags == flags::ACK).expect("ACK на FIN");
    assert_eq!(last.ack, 1007);
    n.poll(Instant::now());
    assert_eq!(n.tcp_flows(), 0, "соединение закрыто");
}

#[test]
fn tcp_large_transfer_respects_window() {
    let (l, port) = listener();
    let mut n = nat();
    let (mut srv, isn) = connect(&mut n, &l, port);
    let payload: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
    let writer = {
        let data = payload.clone();
        std::thread::spawn(move || {
            srv.write_all(&data).expect("write");
            srv
        })
    };
    let mut received = Vec::new();
    let mut next = isn + 1;
    for _ in 0..20_000 {
        n.poll(Instant::now());
        while let Some(f) = n.pop_frame() {
            let Some(s) = as_tcp(&f) else { continue };
            if s.payload.is_empty() {
                continue;
            }
            assert_eq!(s.seq, next, "сегменты идут по порядку");
            assert!(s.payload.len() <= 1460);
            received.extend_from_slice(&s.payload);
            next = next.wrapping_add(u32::try_from(s.payload.len()).expect("len"));
            n.guest_frame(
                &tcp_frame(GATEWAY_IP, port, 1001, next, flags::ACK, &[]),
                Instant::now(),
            );
        }
        if received.len() >= payload.len() {
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    assert_eq!(received.len(), payload.len());
    assert!(received == payload, "данные дошли без искажений");
    drop(writer.join().expect("writer"));
}

#[test]
fn tcp_refused_connection_gets_rst() {
    let (l, port) = listener();
    drop(l);
    let mut n = nat();
    n.guest_frame(
        &tcp_frame(GATEWAY_IP, port, 500, 0, flags::SYN, &[]),
        Instant::now(),
    );
    let rst = wait_seg(&mut n, |s| s.flags & flags::RST != 0).expect("RST");
    assert_eq!(rst.ack, 501);
    n.poll(Instant::now());
    assert_eq!(n.tcp_flows(), 0);
}

#[test]
fn tcp_to_unreachable_subnet_and_unknown_flow_get_rst() {
    let mut n = nat();
    let other = Ipv4Addr::new(10, 0, 2, 99);
    n.guest_frame(
        &tcp_frame(other, 80, 10, 0, flags::SYN, &[]),
        Instant::now(),
    );
    let rst = as_tcp(&n.pop_frame().expect("RST")).expect("tcp");
    assert_eq!(rst.flags, flags::RST | flags::ACK);
    // Сегмент без соединения.
    n.guest_frame(
        &tcp_frame(GATEWAY_IP, 9, 10, 77, flags::ACK, b"x"),
        Instant::now(),
    );
    let rst = as_tcp(&n.pop_frame().expect("RST")).expect("tcp");
    assert_eq!((rst.flags, rst.seq), (flags::RST, 77));
}

#[test]
fn icmp_echo_to_gateway_is_answered_outside_is_not() {
    let mut n = nat();
    let mut req = vec![8, 0, 0, 0, 0, 1, 0, 1, b'a'];
    let c = wire::checksum(&req);
    req[2..4].copy_from_slice(&c.to_be_bytes());
    n.guest_frame(&ip_frame(PROTO_ICMP, GATEWAY_IP, &req), Instant::now());
    let f = n.pop_frame().expect("эхо-ответ");
    let eth = parse_eth(&f).expect("eth");
    let ip = parse_ipv4(eth.payload).expect("ip");
    assert_eq!((ip.src, ip.dst, ip.payload[0]), (GATEWAY_IP, GUEST_IP, 0));
    n.guest_frame(
        &ip_frame(PROTO_ICMP, Ipv4Addr::new(8, 8, 8, 8), &req),
        Instant::now(),
    );
    assert!(n.pop_frame().is_none());
}

#[test]
fn frames_from_other_hosts_and_garbage_are_ignored() {
    let mut n = nat();
    n.guest_frame(&[1, 2, 3], Instant::now());
    // Адрес отправителя не равен адресу гостя.
    let u = wire::build_udp(Ipv4Addr::new(1, 2, 3, 4), GATEWAY_IP, 1, 2, b"x");
    let ip = build_ipv4(Ipv4Addr::new(1, 2, 3, 4), GATEWAY_IP, PROTO_UDP, &u);
    n.guest_frame(
        &build_eth(GATEWAY_MAC, GUEST_MAC, ETHERTYPE_IPV4, &ip),
        Instant::now(),
    );
    assert!(!n.poll(Instant::now()));
    assert!(n.pop_frame().is_none());
}
