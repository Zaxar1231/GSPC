//! TCP: гость говорит с эмулируемым «сервером», а мы терминируем соединение сами и
//! пересылаем байты в обычный сокет хоста (как slirp в QEMU).
//!
//! Повторных передач в сторону гостя нет: канал — память хоста, кадры не теряются (устройство
//! не отбрасывает их, пока у гостя нет буферов приёма), а объём «в полёте» ограничен окном
//! гостя. От гостя повторы принимаются: дубликаты подтверждаются, пропуски — нет (гость
//! повторит сам).

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::Tx;
use crate::wire::{Tcp, TcpOut, flags};

/// Размер сегмента к гостю (MTU 1500 − 40).
pub(crate) const MSS: usize = 1460;
/// Сколько байт от гостя можно держать, пока сокет хоста не принимает.
const TO_HOST_CAP: usize = 256 * 1024;
/// Сколько ждать соединения с сервером.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Соединение без движения закрывается через это время.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// SYN получен, соединение с сервером устанавливается.
    Connecting,
    /// SYN-ACK отправлен, ждём ACK гостя.
    SynRcvd,
    /// Данные идут.
    Established,
}

/// Одно соединение гостя.
pub(crate) struct Flow {
    guest: (Ipv4Addr, u16),
    /// Адрес, с которым гость считает себя соединённым.
    remote: (Ipv4Addr, u16),
    state: State,
    connect: Option<Receiver<std::io::Result<TcpStream>>>,
    stream: Option<TcpStream>,
    /// Следующий ожидаемый от гостя номер последовательности.
    rcv_nxt: u32,
    /// Начальный номер нашей последовательности.
    snd_isn: u32,
    /// Следующий наш номер.
    snd_nxt: u32,
    /// Самый старый неподтверждённый.
    snd_una: u32,
    peer_wscale: u8,
    peer_wnd: u32,
    mss: usize,
    to_host: Vec<u8>,
    guest_fin: bool,
    host_eof: bool,
    fin_sent: bool,
    host_shut: bool,
    last: Instant,
    /// Соединение закончено, запись можно удалить.
    pub(crate) dead: bool,
}

fn seq_le(a: u32, b: u32) -> bool {
    b.wrapping_sub(a) < 0x8000_0000
}

fn spawn_connect(addr: SocketAddr) -> Option<Receiver<std::io::Result<TcpStream>>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("net-connect".into())
        .spawn(move || {
            let _ = tx.send(TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT));
        })
        .ok()?;
    Some(rx)
}

impl Flow {
    /// Новое соединение по SYN гостя `syn`; `target` — куда подключаться на хосте.
    pub(crate) fn new(
        guest: (Ipv4Addr, u16),
        remote: (Ipv4Addr, u16),
        target: SocketAddr,
        syn: &Tcp<'_>,
        isn: u32,
        now: Instant,
    ) -> Self {
        let connect = spawn_connect(target);
        Self {
            guest,
            remote,
            state: State::Connecting,
            dead: connect.is_none(),
            connect,
            stream: None,
            rcv_nxt: syn.seq.wrapping_add(1),
            snd_isn: isn,
            snd_nxt: isn,
            snd_una: isn,
            peer_wscale: syn.wscale.unwrap_or(0),
            peer_wnd: u32::from(syn.window),
            mss: usize::from(syn.mss.unwrap_or(536)).clamp(88, MSS),
            to_host: Vec::new(),
            guest_fin: false,
            host_eof: false,
            fin_sent: false,
            host_shut: false,
            last: now,
        }
    }

    fn adv_window(&self) -> u16 {
        u16::try_from(TO_HOST_CAP.saturating_sub(self.to_host.len()).min(65535)).unwrap_or(0)
    }

    fn send(&self, tx: &mut Tx<'_>, seq: u32, fl: u8, syn_options: Option<u16>, payload: &[u8]) {
        tx.tcp(
            self.remote.0,
            self.guest.0,
            &TcpOut {
                src_port: self.remote.1,
                dst_port: self.guest.1,
                seq,
                ack: self.rcv_nxt,
                flags: fl,
                window: self.adv_window(),
                syn_options,
                payload,
            },
        );
    }

    fn send_ack(&self, tx: &mut Tx<'_>) {
        self.send(tx, self.snd_nxt, flags::ACK, None, &[]);
    }

    fn send_rst(&self, tx: &mut Tx<'_>) {
        self.send(tx, self.snd_nxt, flags::RST | flags::ACK, None, &[]);
    }

    fn send_synack(&mut self, tx: &mut Tx<'_>) {
        self.send(
            tx,
            self.snd_isn,
            flags::SYN | flags::ACK,
            u16::try_from(MSS).ok(),
            &[],
        );
        self.snd_nxt = self.snd_isn.wrapping_add(1);
    }

    /// Сегмент от гостя.
    pub(crate) fn on_guest(&mut self, t: &Tcp<'_>, tx: &mut Tx<'_>, now: Instant) {
        self.last = now;
        if t.flags & flags::RST != 0 {
            self.dead = true;
            return;
        }
        if t.flags & flags::SYN != 0 {
            // Повтор SYN: ответ ещё раз.
            if self.state == State::SynRcvd {
                self.send_synack(tx);
            }
            return;
        }
        if self.state == State::Connecting {
            return;
        }
        self.peer_wnd = u32::from(t.window) << self.peer_wscale;
        if t.flags & flags::ACK != 0 {
            if self.state == State::SynRcvd && t.ack == self.snd_isn.wrapping_add(1) {
                self.state = State::Established;
            }
            if seq_le(self.snd_una, t.ack) && seq_le(t.ack, self.snd_nxt) {
                self.snd_una = t.ack;
            }
        }
        if self.state != State::Established {
            return;
        }
        let len = t.payload.len();
        // Сколько байт сегмента уже получено (повтор) и нет ли пропуска перед ним.
        let behind = self.rcv_nxt.wrapping_sub(t.seq);
        if behind >= 0x8000_0000 && (len > 0 || t.flags & flags::FIN != 0) {
            // Пропуск: подтверждаем то, что есть, гость повторит.
            self.send_ack(tx);
            return;
        }
        let skip = usize::try_from(behind).unwrap_or(usize::MAX);
        let fresh = t.payload.get(skip..).unwrap_or(&[]);
        let mut respond = len > 0 && fresh.is_empty();
        if !fresh.is_empty() {
            if self.to_host.len() + fresh.len() > TO_HOST_CAP {
                // Буфер полон: не принимаем, гость повторит позже.
                self.send_ack(tx);
                return;
            }
            self.to_host.extend_from_slice(fresh);
            self.rcv_nxt = self
                .rcv_nxt
                .wrapping_add(u32::try_from(fresh.len()).unwrap_or(0));
            respond = true;
        }
        let end = t.seq.wrapping_add(u32::try_from(len).unwrap_or(0));
        if t.flags & flags::FIN != 0 && end == self.rcv_nxt && !self.guest_fin {
            self.guest_fin = true;
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            respond = true;
        }
        if respond {
            self.send_ack(tx);
        }
    }

    /// Работа со стороны хоста; `true` — что-то сделано. `budget` — сколько сегментов гостю
    /// можно отдать за вызов.
    pub(crate) fn poll(&mut self, tx: &mut Tx<'_>, now: Instant, budget: usize) -> bool {
        let mut progress = false;
        if self.state == State::Connecting {
            let result = match self.connect.as_ref().map(Receiver::try_recv) {
                Some(Err(TryRecvError::Empty)) => None,
                Some(Ok(r)) => Some(r),
                _ => Some(Err(std::io::Error::other("поток соединения потерян"))),
            };
            if let Some(r) = result {
                self.connect = None;
                progress = true;
                match r.and_then(|s| {
                    s.set_nonblocking(true)?;
                    let _ = s.set_nodelay(true);
                    Ok(s)
                }) {
                    Ok(s) => {
                        self.stream = Some(s);
                        self.state = State::SynRcvd;
                        self.send_synack(tx);
                    }
                    Err(e) => {
                        tracing::debug!(remote = ?self.remote, error = %e, "соединение не установлено");
                        // RST|ACK на SYN: seq 0.
                        self.send(tx, 0, flags::RST | flags::ACK, None, &[]);
                        self.dead = true;
                    }
                }
            }
        }
        if self.state != State::Established {
            if now.duration_since(self.last) > IDLE_TIMEOUT {
                self.dead = true;
            }
            return progress;
        }
        let Some(mut stream) = self.stream.take() else {
            self.dead = true;
            return progress;
        };
        progress |= self.pump(&mut stream, tx, now, budget);
        self.stream = Some(stream);
        progress
    }

    /// Обмен байтами с сокетом хоста для установленного соединения.
    fn pump(
        &mut self,
        stream: &mut TcpStream,
        tx: &mut Tx<'_>,
        now: Instant,
        budget: usize,
    ) -> bool {
        let mut progress = false;
        // Гость → сервер.
        while !self.to_host.is_empty() {
            match stream.write(&self.to_host) {
                Ok(0) => break,
                Ok(n) => {
                    self.to_host.drain(..n);
                    progress = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    self.send_rst(tx);
                    self.dead = true;
                    return true;
                }
            }
        }
        if self.guest_fin && self.to_host.is_empty() && !self.host_shut {
            let _ = stream.shutdown(Shutdown::Write);
            self.host_shut = true;
            progress = true;
        }
        // Сервер → гость.
        let mut buf = [0u8; MSS];
        let mut segments = 0;
        while !self.host_eof && segments < budget {
            let inflight = usize::try_from(self.snd_nxt.wrapping_sub(self.snd_una)).unwrap_or(0);
            let room = usize::try_from(self.peer_wnd)
                .unwrap_or(0)
                .saturating_sub(inflight);
            if room == 0 {
                break;
            }
            let want = room.min(self.mss).min(MSS);
            match stream.read(buf.get_mut(..want).unwrap_or(&mut [])) {
                Ok(0) => self.host_eof = true,
                Ok(n) => {
                    let data = buf.get(..n).unwrap_or(&[]);
                    self.send(tx, self.snd_nxt, flags::ACK | flags::PSH, None, data);
                    self.snd_nxt = self.snd_nxt.wrapping_add(u32::try_from(n).unwrap_or(0));
                    self.last = now;
                    segments += 1;
                    progress = true;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    self.send_rst(tx);
                    self.dead = true;
                    return true;
                }
            }
        }
        if self.host_eof && !self.fin_sent {
            self.send(tx, self.snd_nxt, flags::FIN | flags::ACK, None, &[]);
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            self.fin_sent = true;
            progress = true;
        }
        if self.fin_sent && self.guest_fin && self.snd_una == self.snd_nxt {
            self.dead = true;
        }
        if now.duration_since(self.last) > IDLE_TIMEOUT {
            self.dead = true;
        }
        progress
    }
}

/// RST в ответ на сегмент без соединения.
pub(crate) fn rst_for(
    t: &Tcp<'_>,
    local: (Ipv4Addr, u16),
    guest: (Ipv4Addr, u16),
    tx: &mut Tx<'_>,
) {
    let len = u32::try_from(t.payload.len()).unwrap_or(0)
        + u32::from(t.flags & flags::SYN != 0)
        + u32::from(t.flags & flags::FIN != 0);
    let (seq, ack, fl) = if t.flags & flags::ACK != 0 {
        (t.ack, 0, flags::RST)
    } else {
        (0, t.seq.wrapping_add(len), flags::RST | flags::ACK)
    };
    tx.tcp(
        local.0,
        guest.0,
        &TcpOut {
            src_port: local.1,
            dst_port: guest.1,
            seq,
            ack,
            flags: fl,
            window: 0,
            syn_options: None,
            payload: &[],
        },
    );
}
