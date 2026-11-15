//! Хост-сторона гостевого агента: служба vsock на порту [`rustdroid_agent::PORT`] и
//! управляющий объект [`AgentHost`] (активное приложение, установка APK).
//!
//! Протокол — `rustdroid_agent::proto`. Агент в госте подключается к хосту, говорит
//! `Hello`, сообщает активное приложение (`Foreground`) и выполняет установки.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use rustdroid_agent::proto::{CHUNK, Decoder, Message, PORT, VERSION, encode};

use crate::vsock::{VsockOutbox, VsockService};

/// Порт vsock хоста, на котором служба ждёт агента.
pub const AGENT_PORT: u32 = PORT;

/// Итог установки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// Установка удалась.
    pub ok: bool,
    /// Вывод `pm install` или причина отказа.
    pub message: String,
}

/// Ошибки управления агентом.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentError {
    /// Агент в госте не подключён.
    NotConnected,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("агент в госте не подключён"),
        }
    }
}

impl std::error::Error for AgentError {}

#[derive(Debug, Default)]
struct State {
    connected: AtomicBool,
    foreground: Mutex<String>,
    installs: Mutex<HashMap<u32, Sender<InstallOutcome>>>,
    next_id: AtomicU32,
}

/// Управление агентом: можно клонировать и использовать из любых потоков.
#[derive(Debug, Clone)]
pub struct AgentHost {
    state: Arc<State>,
    outbox: Arc<VsockOutbox>,
}

impl AgentHost {
    /// Агент подключён и поздоровался.
    #[must_use]
    pub fn connected(&self) -> bool {
        self.state.connected.load(Ordering::Acquire)
    }

    /// Пакет приложения на переднем плане (пусто — нет приложения или агент не подключён).
    #[must_use]
    pub fn foreground(&self) -> String {
        self.state
            .foreground
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Начинает установку APK `data` (`name` — имя файла для сообщений); итог придёт в
    /// возвращённый приёмник.
    ///
    /// # Errors
    /// [`AgentError::NotConnected`], если агент не подключён.
    pub fn install(&self, name: &str, data: &[u8]) -> Result<Receiver<InstallOutcome>, AgentError> {
        if !self.connected() {
            return Err(AgentError::NotConnected);
        }
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.state
            .installs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, tx);
        self.outbox.send(
            PORT,
            encode(&Message::InstallBegin {
                id,
                size: data.len() as u64,
                name: name.to_owned(),
            }),
        );
        for chunk in data.chunks(CHUNK) {
            self.outbox.send(
                PORT,
                encode(&Message::InstallChunk {
                    id,
                    data: chunk.to_vec(),
                }),
            );
        }
        self.outbox.send(PORT, encode(&Message::InstallEnd { id }));
        Ok(rx)
    }
}

/// Служба vsock агента.
#[derive(Debug)]
pub struct AgentService {
    state: Arc<State>,
    decoders: HashMap<u32, Decoder>,
}

/// Создаёт службу (для регистрации на порту [`PORT`]) и управляющий объект.
#[must_use]
pub fn new(outbox: Arc<VsockOutbox>) -> (AgentService, AgentHost) {
    let state = Arc::new(State::default());
    (
        AgentService {
            state: Arc::clone(&state),
            decoders: HashMap::new(),
        },
        AgentHost { state, outbox },
    )
}

impl AgentService {
    fn handle(&self, m: Message) -> Vec<u8> {
        match m {
            Message::Hello { version } => {
                tracing::info!(version, host = VERSION, "агент гостя подключён");
                self.state.connected.store(true, Ordering::Release);
                encode(&Message::HelloAck { version: VERSION })
            }
            Message::Foreground { package } => {
                let mut cur = self
                    .state
                    .foreground
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if *cur != package {
                    tracing::info!(package = %package, "активное приложение гостя");
                    *cur = package;
                }
                Vec::new()
            }
            Message::InstallResult { id, ok, message } => {
                tracing::info!(id, ok, message = %message, "результат установки APK");
                if let Some(tx) = self
                    .state
                    .installs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = tx.send(InstallOutcome { ok, message });
                }
                Vec::new()
            }
            Message::Ping { token } => encode(&Message::Pong { token }),
            // Остальное — сообщения хоста; от агента их не ждём.
            _ => Vec::new(),
        }
    }
}

impl VsockService for AgentService {
    fn on_data(&mut self, guest_port: u32, data: &[u8]) -> Vec<u8> {
        let decoded = self.decoders.entry(guest_port).or_default().feed(data);
        match decoded {
            Ok(messages) => messages.into_iter().flat_map(|m| self.handle(m)).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "агент гостя: поток сообщений повреждён, сброс");
                self.decoders.remove(&guest_port);
                Vec::new()
            }
        }
    }

    fn on_close(&mut self, guest_port: u32) {
        self.decoders.remove(&guest_port);
        self.state.connected.store(false, Ordering::Release);
        self.state
            .foreground
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        // Установки, которые не успели закончиться, считаются проваленными.
        for (_, tx) in self
            .state
            .installs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
        {
            let _ = tx.send(InstallOutcome {
                ok: false,
                message: "агент отключился до конца установки".into(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (AgentService, AgentHost, Arc<VsockOutbox>) {
        let outbox = VsockOutbox::new();
        let (svc, host) = new(Arc::clone(&outbox));
        (svc, host, outbox)
    }

    fn feed(svc: &mut AgentService, m: &Message) -> Vec<Message> {
        let reply = svc.on_data(50_000, &encode(m));
        Decoder::default().feed(&reply).unwrap()
    }

    #[test]
    fn hello_connects_and_foreground_is_tracked() {
        let (mut svc, host, _) = setup();
        assert!(!host.connected());
        assert_eq!(
            feed(&mut svc, &Message::Hello { version: 1 }),
            vec![Message::HelloAck { version: VERSION }]
        );
        assert!(host.connected());
        feed(
            &mut svc,
            &Message::Foreground {
                package: "com.game".into(),
            },
        );
        assert_eq!(host.foreground(), "com.game");
        assert_eq!(
            feed(&mut svc, &Message::Ping { token: 5 }),
            vec![Message::Pong { token: 5 }]
        );
        svc.on_close(50_000);
        assert!(!host.connected());
        assert_eq!(host.foreground(), "");
    }

    #[test]
    fn install_requires_agent_and_streams_chunks() {
        let (mut svc, host, outbox) = setup();
        assert_eq!(
            host.install("a.apk", b"x").unwrap_err(),
            AgentError::NotConnected
        );
        feed(&mut svc, &Message::Hello { version: 1 });
        let apk: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
        let rx = host.install("a.apk", &apk).unwrap();
        // Всё, что хост отправил агенту, собирается в сообщения.
        let sent = outbox.take();
        assert!(sent.iter().all(|(port, _)| *port == PORT));
        let bytes: Vec<u8> = sent.into_iter().flat_map(|(_, d)| d).collect();
        let msgs = Decoder::default().feed(&bytes).unwrap();
        let Some(Message::InstallBegin { id, size, name }) = msgs.first().cloned() else {
            panic!("нет InstallBegin");
        };
        assert_eq!((size, name.as_str()), (70_000, "a.apk"));
        let mut got = Vec::new();
        for m in &msgs[1..msgs.len() - 1] {
            let Message::InstallChunk { id: cid, data } = m else {
                panic!("ожидался кусок");
            };
            assert_eq!(*cid, id);
            assert!(data.len() <= CHUNK);
            got.extend_from_slice(data);
        }
        assert_eq!(got, apk);
        assert_eq!(msgs.last(), Some(&Message::InstallEnd { id }));
        // Итог доходит до ожидающего.
        feed(
            &mut svc,
            &Message::InstallResult {
                id,
                ok: true,
                message: "Success".into(),
            },
        );
        assert_eq!(
            rx.recv().unwrap(),
            InstallOutcome {
                ok: true,
                message: "Success".into()
            }
        );
    }

    #[test]
    fn disconnect_fails_pending_installs() {
        let (mut svc, host, _) = setup();
        feed(&mut svc, &Message::Hello { version: 1 });
        let rx = host.install("a.apk", b"data").unwrap();
        svc.on_close(50_000);
        let out = rx.recv().unwrap();
        assert!(!out.ok);
    }

    #[test]
    fn corrupt_stream_is_dropped_not_fatal() {
        let (mut svc, host, _) = setup();
        // Нулевая длина кадра: поток сбрасывается, дальше принимаем заново.
        assert!(svc.on_data(1, &[0, 0, 0, 0]).is_empty());
        assert_eq!(
            Decoder::default()
                .feed(&svc.on_data(1, &encode(&Message::Hello { version: 1 })))
                .unwrap(),
            vec![Message::HelloAck { version: VERSION }]
        );
        assert!(host.connected());
    }
}
