//! UDP: каждому потоку гостя (порт гостя, адрес и порт назначения) — свой сокет хоста.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::Tx;

/// Максимум потоков.
const MAX_FLOWS: usize = 256;
/// Поток без движения закрывается через это время.
const IDLE: Duration = Duration::from_secs(60);
/// Самая длинная датаграмма, помещающаяся в кадр (MTU 1500 − 20 − 8).
const MAX_DATAGRAM: usize = 1472;

struct Flow {
    sock: UdpSocket,
    /// Адрес, с которым гость считает себя связанным.
    remote: (Ipv4Addr, u16),
    guest_port: u16,
    last: Instant,
}

/// Потоки UDP.
#[derive(Default)]
pub(crate) struct Flows {
    map: HashMap<(u16, Ipv4Addr, u16), Flow>,
}

impl Flows {
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Датаграмма гостя.
    pub(crate) fn send(
        &mut self,
        guest_port: u16,
        remote: (Ipv4Addr, u16),
        target: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        let key = (guest_port, remote.0, remote.1);
        if !self.map.contains_key(&key) {
            if self.map.len() >= MAX_FLOWS {
                return;
            }
            let bound = UdpSocket::bind("0.0.0.0:0").and_then(|s| {
                s.set_nonblocking(true)?;
                s.connect(target)?;
                Ok(s)
            });
            match bound {
                Ok(sock) => {
                    self.map.insert(
                        key,
                        Flow {
                            sock,
                            remote,
                            guest_port,
                            last: now,
                        },
                    );
                }
                Err(e) => {
                    tracing::debug!(error = %e, %target, "сокет UDP не создан");
                    return;
                }
            }
        }
        if let Some(f) = self.map.get_mut(&key) {
            f.last = now;
            // Полный буфер сокета — обычная потеря UDP.
            let _ = f.sock.send(payload);
        }
    }

    /// Читает ответы, отдаёт их гостю. `true` — были данные.
    pub(crate) fn poll(&mut self, guest_ip: Ipv4Addr, tx: &mut Tx<'_>, now: Instant) -> bool {
        let mut progress = false;
        let mut buf = [0u8; 2048];
        for f in self.map.values_mut() {
            for _ in 0..16 {
                match f.sock.recv(&mut buf) {
                    Ok(n) if n <= MAX_DATAGRAM => {
                        f.last = now;
                        progress = true;
                        tx.udp(
                            f.remote,
                            (guest_ip, f.guest_port),
                            buf.get(..n).unwrap_or(&[]),
                        );
                    }
                    Ok(_) => {}
                    // Отказ «порт недоступен» приходит ошибкой следующего чтения — пропускаем.
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => {}
                }
            }
        }
        progress
    }

    /// Закрывает давно молчащие потоки.
    pub(crate) fn expire(&mut self, now: Instant) {
        self.map.retain(|_, f| now.duration_since(f.last) < IDLE);
    }
}
