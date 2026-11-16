//! Сеть гостя без драйверов и прав администратора: NAT в пользовательском режиме.
//!
//! Гость видит обычную сеть Ethernet `10.0.2.0/24`: адрес `10.0.2.15` (его выдаёт встроенный
//! DHCP), шлюз `10.0.2.2` (он же — сам хост: соединение на `10.0.2.2` попадает на `127.0.0.1`),
//! DNS `10.0.2.3` (запросы уходят на DNS-сервер хоста). Кадры Ethernet от гостя разбираются
//! здесь; TCP и UDP превращаются в обычные сокеты хоста, ответы собираются обратно в кадры.
//!
//! Ядро ([`Nat`]) — чистая логика без потоков: [`Nat::guest_frame`] принимает кадр,
//! [`Nat::poll`] опрашивает сокеты хоста, [`Nat::pop_frame`] отдаёт кадры гостю. Опрос и
//! будильник приёма — забота вызывающего (устройство virtio-net).
//!
//! Не поддерживается: IPv6, фрагментированные IP-пакеты, ICMP-эхо наружу (нужны привилегии),
//! входящие соединения с хоста в гость.

mod dhcp;
mod tcp;
mod udp;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Instant;

use wire::{
    BROADCAST, ETHERTYPE_ARP, ETHERTYPE_IPV4, Mac, PROTO_ICMP, PROTO_TCP, PROTO_UDP, TcpOut,
};

/// Адрес гостя по умолчанию.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
/// Шлюз (и хост) по умолчанию.
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
/// DNS по умолчанию.
pub const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
/// MAC шлюза.
pub const GATEWAY_MAC: Mac = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

/// Сколько кадров можно держать для гостя, прежде чем перестать читать сокеты хоста.
const OUT_HIGH: usize = 512;
/// Сколько соединений TCP одновременно.
const MAX_TCP: usize = 1024;

/// Настройки NAT.
#[derive(Debug, Clone)]
pub struct NatConfig {
    /// Адрес гостя.
    pub guest_ip: Ipv4Addr,
    /// Шлюз.
    pub gateway_ip: Ipv4Addr,
    /// DNS, который видит гость.
    pub dns_ip: Ipv4Addr,
    /// Маска сети.
    pub netmask: Ipv4Addr,
    /// DNS-сервер хоста, куда уходят запросы гостя.
    pub upstream_dns: SocketAddr,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self {
            guest_ip: GUEST_IP,
            gateway_ip: GATEWAY_IP,
            dns_ip: DNS_IP,
            netmask: Ipv4Addr::new(255, 255, 255, 0),
            upstream_dns: SocketAddr::from(([8, 8, 8, 8], 53)),
        }
    }
}

/// Выбирает DNS-сервер хоста из списка адресов-строк: первый годный IPv4 (не локальный, не
/// нулевой, не link-local, не multicast).
#[must_use]
pub fn pick_dns<'a>(candidates: impl IntoIterator<Item = &'a str>) -> Option<Ipv4Addr> {
    candidates
        .into_iter()
        .filter_map(|c| c.trim().parse::<Ipv4Addr>().ok())
        .find(|ip| {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast())
        })
}

/// Очередь кадров к гостю с адресами Ethernet.
pub(crate) struct Tx<'a> {
    out: &'a mut VecDeque<Vec<u8>>,
    guest_mac: Mac,
}

impl Tx<'_> {
    pub(crate) fn ip(&mut self, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) {
        let ip = wire::build_ipv4(src, dst, proto, payload);
        self.out.push_back(wire::build_eth(
            self.guest_mac,
            GATEWAY_MAC,
            ETHERTYPE_IPV4,
            &ip,
        ));
    }

    pub(crate) fn tcp(&mut self, src: Ipv4Addr, dst: Ipv4Addr, seg: &TcpOut<'_>) {
        let b = wire::build_tcp(src, dst, seg);
        self.ip(src, dst, PROTO_TCP, &b);
    }

    pub(crate) fn udp(&mut self, src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), payload: &[u8]) {
        let b = wire::build_udp(src.0, dst.0, src.1, dst.1, payload);
        self.ip(src.0, dst.0, PROTO_UDP, &b);
    }
}

type TcpKey = (u16, Ipv4Addr, u16);

/// NAT гостя.
pub struct Nat {
    cfg: NatConfig,
    guest_mac: Option<Mac>,
    out: VecDeque<Vec<u8>>,
    tcp: HashMap<TcpKey, tcp::Flow>,
    udp: udp::Flows,
    isn: u32,
}

impl std::fmt::Debug for Nat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nat")
            .field("tcp", &self.tcp.len())
            .field("udp", &self.udp.len())
            .field("queued", &self.out.len())
            .finish_non_exhaustive()
    }
}

impl Nat {
    /// Новый NAT.
    #[must_use]
    pub fn new(cfg: NatConfig) -> Self {
        Self {
            cfg,
            guest_mac: None,
            out: VecDeque::new(),
            tcp: HashMap::new(),
            udp: udp::Flows::default(),
            isn: 0x1000_0000,
        }
    }

    /// Кадр для гостя, если есть.
    pub fn pop_frame(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// Сколько кадров ждёт гостя.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.out.len()
    }

    /// Вернуть кадр в начало очереди (у гостя не оказалось места).
    pub fn push_back_front(&mut self, frame: Vec<u8>) {
        self.out.push_front(frame);
    }

    fn in_subnet(&self, ip: Ipv4Addr) -> bool {
        let m = u32::from(self.cfg.netmask);
        u32::from(ip) & m == u32::from(self.cfg.gateway_ip) & m
    }

    /// Куда на хосте отправить то, что гость адресовал `ip:port` (`None` — никуда).
    fn host_target(&self, ip: Ipv4Addr, port: u16, dns: bool) -> Option<SocketAddr> {
        if dns && ip == self.cfg.dns_ip {
            return Some(self.cfg.upstream_dns);
        }
        if ip == self.cfg.gateway_ip {
            return Some(SocketAddr::from(([127, 0, 0, 1], port)));
        }
        if self.in_subnet(ip)
            || ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_multicast()
            || ip.is_broadcast()
        {
            return None;
        }
        Some(SocketAddr::from((ip, port)))
    }

    /// Кадр Ethernet от гостя.
    pub fn guest_frame(&mut self, frame: &[u8], now: Instant) {
        let Some(eth) = wire::parse_eth(frame) else {
            return;
        };
        if eth.src != [0; 6] {
            self.guest_mac = Some(eth.src);
        }
        match eth.ethertype {
            ETHERTYPE_ARP => self.on_arp(eth.src, eth.payload),
            ETHERTYPE_IPV4 => self.on_ipv4(eth.src, eth.payload, now),
            _ => {}
        }
    }

    fn tx(&mut self) -> Tx<'_> {
        Tx {
            out: &mut self.out,
            guest_mac: self.guest_mac.unwrap_or(BROADCAST),
        }
    }

    fn on_arp(&mut self, src_mac: Mac, p: &[u8]) {
        let Some(a) = wire::parse_arp(p) else {
            return;
        };
        if a.op != 1 || a.tpa == self.cfg.guest_ip || !self.in_subnet(a.tpa) {
            return;
        }
        let reply = wire::build_arp(&wire::Arp {
            op: 2,
            sha: GATEWAY_MAC,
            spa: a.tpa,
            tha: a.sha,
            tpa: a.spa,
        });
        self.out
            .push_back(wire::build_eth(src_mac, GATEWAY_MAC, ETHERTYPE_ARP, &reply));
    }

    fn on_ipv4(&mut self, src_mac: Mac, p: &[u8], now: Instant) {
        let Some(ip) = wire::parse_ipv4(p) else {
            return;
        };
        match ip.proto {
            PROTO_UDP => {
                if let Some(u) = wire::parse_udp(ip.payload) {
                    self.on_udp(src_mac, ip.src, ip.dst, &u, now);
                }
            }
            PROTO_TCP => {
                if let Some(t) = wire::parse_tcp(ip.payload) {
                    self.on_tcp(ip.src, ip.dst, &t, now);
                }
            }
            PROTO_ICMP => {
                // Эхо отвечаем только за шлюз и DNS: наружу ICMP без привилегий не отправить.
                if (ip.dst == self.cfg.gateway_ip || ip.dst == self.cfg.dns_ip)
                    && let Some(reply) = wire::icmp_echo_reply(ip.payload)
                {
                    self.tx().ip(ip.dst, ip.src, PROTO_ICMP, &reply);
                }
            }
            _ => {}
        }
    }

    fn on_udp(
        &mut self,
        src_mac: Mac,
        src: Ipv4Addr,
        dst: Ipv4Addr,
        u: &wire::Udp<'_>,
        now: Instant,
    ) {
        if u.dst_port == 67 && u.src_port == 68 {
            self.on_dhcp(src_mac, u.payload);
            return;
        }
        if src != self.cfg.guest_ip {
            return;
        }
        let Some(target) = self.host_target(dst, u.dst_port, u.dst_port == 53) else {
            return;
        };
        self.udp
            .send(u.src_port, (dst, u.dst_port), target, u.payload, now);
    }

    fn on_dhcp(&mut self, src_mac: Mac, payload: &[u8]) {
        let Some(req) = dhcp::parse(payload) else {
            return;
        };
        let kind = match req.kind {
            dhcp::DISCOVER => dhcp::OFFER,
            dhcp::REQUEST => dhcp::ACK,
            _ => return,
        };
        let lease = dhcp::Lease {
            guest: self.cfg.guest_ip,
            gateway: self.cfg.gateway_ip,
            netmask: self.cfg.netmask,
            dns: self.cfg.dns_ip,
        };
        tracing::info!(
            kind = if kind == dhcp::OFFER { "OFFER" } else { "ACK" },
            guest = %self.cfg.guest_ip,
            "DHCP: гостю выдан адрес"
        );
        let body = dhcp::reply(&req, kind, &lease);
        let udp = wire::build_udp(self.cfg.gateway_ip, Ipv4Addr::BROADCAST, 67, 68, &body);
        let ip = wire::build_ipv4(self.cfg.gateway_ip, Ipv4Addr::BROADCAST, PROTO_UDP, &udp);
        self.out
            .push_back(wire::build_eth(src_mac, GATEWAY_MAC, ETHERTYPE_IPV4, &ip));
    }

    fn on_tcp(&mut self, src: Ipv4Addr, dst: Ipv4Addr, t: &wire::Tcp<'_>, now: Instant) {
        if src != self.cfg.guest_ip {
            return;
        }
        let key = (t.src_port, dst, t.dst_port);
        let guest = (src, t.src_port);
        let remote = (dst, t.dst_port);
        if let Some(flow) = self.tcp.get_mut(&key) {
            let mut tx = Tx {
                out: &mut self.out,
                guest_mac: self.guest_mac.unwrap_or(BROADCAST),
            };
            flow.on_guest(t, &mut tx, now);
            return;
        }
        let new_conn = t.flags & wire::flags::SYN != 0 && t.flags & wire::flags::ACK == 0;
        if !new_conn {
            if t.flags & wire::flags::RST == 0 {
                let mut tx = self.tx();
                tcp::rst_for(t, remote, guest, &mut tx);
            }
            return;
        }
        let target = self.host_target(dst, t.dst_port, false);
        let (Some(target), true) = (target, self.tcp.len() < MAX_TCP) else {
            let mut tx = self.tx();
            tcp::rst_for(t, remote, guest, &mut tx);
            return;
        };
        self.isn = self.isn.wrapping_add(0x0001_0000 + u32::from(t.src_port));
        let flow = tcp::Flow::new(guest, remote, target, t, self.isn, now);
        self.tcp.insert(key, flow);
    }

    /// Опрос сокетов хоста. `true` — были события.
    pub fn poll(&mut self, now: Instant) -> bool {
        let mut progress = false;
        let guest_mac = self.guest_mac.unwrap_or(BROADCAST);
        let congested = self.out.len() >= OUT_HIGH;
        let guest_ip = self.cfg.guest_ip;
        let mut tx = Tx {
            out: &mut self.out,
            guest_mac,
        };
        // Читать из сокетов хоста можно, пока гость успевает забирать кадры.
        for flow in self.tcp.values_mut() {
            let budget = if congested { 0 } else { 32 };
            progress |= flow.poll(&mut tx, now, budget);
        }
        self.tcp.retain(|_, f| !f.dead);
        if !congested {
            progress |= self.udp.poll(guest_ip, &mut tx, now);
        }
        self.udp.expire(now);
        progress
    }

    /// Число активных соединений TCP (для логов и тестов).
    #[must_use]
    pub fn tcp_flows(&self) -> usize {
        self.tcp.len()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dns_tests {
    use super::*;

    #[test]
    fn picks_first_usable_ipv4() {
        assert_eq!(
            pick_dns([
                "",
                "127.0.0.1",
                "0.0.0.0",
                "169.254.1.1",
                "fe80::1",
                "192.168.1.1",
                "8.8.8.8"
            ]),
            Some(Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(pick_dns(["junk", "::1"]), None);
        assert_eq!(pick_dns([" 1.1.1.1 "]), Some(Ipv4Addr::new(1, 1, 1, 1)));
    }
}
