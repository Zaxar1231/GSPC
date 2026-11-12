//! Службы хоста для HAL Cuttlefish на консолях virtio.
//!
//! HAL Cuttlefish общаются с хостом через `/dev/hvcN` сообщениями транспорта Cuttlefish
//! (`cuttlefish::transport::SharedFdChannel`): 4 байта — номер команды (старший бит — флаг
//! ответа), 4 байта — длина данных, затем данные (little-endian). Если хост молчит, HAL
//! блокируются: HAL датчиков ждёт ответа на `list-sensors`, и system_server, который создаёт
//! `SensorManager` при загрузке, через 66 секунд убивает свой Watchdog.
//!
//! [`ServiceEndpoint`] подключается как вывод консоли: собирает сообщения из потока гостя,
//! передаёт их [`HostService`] и кладёт ответы во ввод той же консоли.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::console::ConsoleInput;

/// Размер заголовка сообщения.
pub const HEADER_LEN: usize = 8;
/// Флаг ответа в слове команды.
const RESPONSE_BIT: u32 = 1 << 31;
/// Предел данных одного сообщения (защита от мусора в потоке).
const MAX_PAYLOAD: usize = 64 * 1024;

/// Сообщение транспорта Cuttlefish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Номер команды (31 бит).
    pub command: u32,
    /// Это ответ.
    pub is_response: bool,
    /// Данные.
    pub payload: Vec<u8>,
}

impl Message {
    /// Ответ на команду `command` с данными `payload`.
    #[must_use]
    pub fn response(command: u32, payload: Vec<u8>) -> Self {
        Self {
            command,
            is_response: true,
            payload,
        }
    }

    /// Кодирование в байты потока.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let word = (self.command & !RESPONSE_BIT) | if self.is_response { RESPONSE_BIT } else { 0 };
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&word.to_le_bytes());
        #[allow(clippy::cast_possible_truncation, reason = "payload ≤ MAX_PAYLOAD")]
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Служба хоста: отвечает на сообщения гостя.
pub trait HostService: Send {
    /// Обрабатывает сообщение гостя, возвращает ответ (если нужен).
    fn handle(&mut self, msg: &Message) -> Option<Message>;
}

/// Вывод консоли, за которым стоит служба хоста.
pub struct ServiceEndpoint<S> {
    service: S,
    input: Arc<ConsoleInput>,
    buf: Vec<u8>,
}

impl<S> std::fmt::Debug for ServiceEndpoint<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceEndpoint")
            .field("buffered", &self.buf.len())
            .finish_non_exhaustive()
    }
}

impl<S: HostService> ServiceEndpoint<S> {
    /// Служба `service`, ответы — во ввод консоли `input`.
    #[must_use]
    pub fn new(service: S, input: Arc<ConsoleInput>) -> Self {
        Self {
            service,
            input,
            buf: Vec::new(),
        }
    }

    fn drain_messages(&mut self) {
        loop {
            let Some(header) = self.buf.get(..HEADER_LEN) else {
                return;
            };
            let word = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
            if len > MAX_PAYLOAD {
                tracing::warn!(
                    len,
                    "служба консоли: слишком длинное сообщение, поток сброшен"
                );
                self.buf.clear();
                return;
            }
            let Some(payload) = self.buf.get(HEADER_LEN..HEADER_LEN + len) else {
                return;
            };
            let msg = Message {
                command: word & !RESPONSE_BIT,
                is_response: word & RESPONSE_BIT != 0,
                payload: payload.to_vec(),
            };
            self.buf.drain(..HEADER_LEN + len);
            if let Some(resp) = self.service.handle(&msg) {
                self.input.push(&resp.encode());
            }
        }
    }
}

impl<S: HostService> Write for ServiceEndpoint<S> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        self.drain_messages();
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Управляющий канал HAL датчиков (`/dev/hvc18`, протокол goldfish поверх транспорта).
///
/// На `list-sensors` отвечает маской датчиков хоста; остальные команды (`time:`, `set-delay:`,
/// `set:`) принимает без ответа. Пока датчиков нет (маска 0); акселерометр и гироскоп для игр
/// появятся вместе с поворотом экрана.
#[derive(Debug, Default)]
pub struct SensorsControl {
    /// Маска датчиков хоста (бит — номер датчика goldfish).
    pub mask: u32,
}

impl HostService for SensorsControl {
    fn handle(&mut self, msg: &Message) -> Option<Message> {
        let text = String::from_utf8_lossy(&msg.payload);
        let text = text.trim_end_matches('\0');
        tracing::debug!(command = text, "датчики: команда гостя");
        (text == "list-sensors")
            .then(|| Message::response(msg.command, self.mask.to_string().into_bytes()))
    }
}

/// HAL oemlock (`/dev/hvc10`): поля — номера команд, запрос без данных — чтение, с одним
/// байтом — запись; ответ всегда один байт `bool`.
#[derive(Debug)]
pub struct OemLock {
    fields: HashMap<u32, bool>,
}

impl Default for OemLock {
    fn default() -> Self {
        // Поле 0 — «разблокировка разрешена оператором»; устройство не заблокировано.
        Self {
            fields: HashMap::from([(0, true)]),
        }
    }
}

impl HostService for OemLock {
    fn handle(&mut self, msg: &Message) -> Option<Message> {
        if let Some(&v) = msg.payload.first() {
            self.fields.insert(msg.command, v != 0);
        }
        let value = self.fields.get(&msg.command).copied().unwrap_or(false);
        Some(Message::response(msg.command, vec![u8::from(value)]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: u32, payload: &[u8]) -> Vec<u8> {
        Message {
            command,
            is_response: false,
            payload: payload.to_vec(),
        }
        .encode()
    }

    fn take_response(input: &ConsoleInput) -> Message {
        let bytes = input.take_all();
        let word = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), HEADER_LEN + len);
        Message {
            command: word & !RESPONSE_BIT,
            is_response: word & RESPONSE_BIT != 0,
            payload: bytes[8..].to_vec(),
        }
    }

    #[test]
    fn sensors_list_reply_even_when_split_across_writes() {
        let input = Arc::new(ConsoleInput::default());
        let mut ep = ServiceEndpoint::new(SensorsControl { mask: 5 }, input.clone());
        let req = request(0, b"list-sensors");
        ep.write_all(&req[..5]).unwrap();
        assert!(input.take_all().is_empty());
        ep.write_all(&req[5..]).unwrap();
        let r = take_response(&input);
        assert_eq!(r, Message::response(0, b"5".to_vec()));
        // Команды без ответа.
        ep.write_all(&request(0, b"set-delay:20")).unwrap();
        assert!(input.take_all().is_empty());
    }

    #[test]
    fn oemlock_reads_and_writes_fields() {
        let input = Arc::new(ConsoleInput::default());
        let mut ep = ServiceEndpoint::new(OemLock::default(), input.clone());
        ep.write_all(&request(0, &[])).unwrap();
        assert_eq!(take_response(&input).payload, [1]);
        ep.write_all(&request(3, &[])).unwrap();
        assert_eq!(take_response(&input).payload, [0]);
        ep.write_all(&request(3, &[1])).unwrap();
        let r = take_response(&input);
        assert!(r.is_response);
        assert_eq!((r.command, r.payload), (3, vec![1]));
        ep.write_all(&request(3, &[])).unwrap();
        assert_eq!(take_response(&input).payload, [1]);
    }

    #[test]
    fn oversized_message_resets_stream() {
        let input = Arc::new(ConsoleInput::default());
        let mut ep = ServiceEndpoint::new(OemLock::default(), input.clone());
        let mut bad = 0_u32.to_le_bytes().to_vec();
        bad.extend_from_slice(&(MAX_PAYLOAD as u32 + 1).to_le_bytes());
        ep.write_all(&bad).unwrap();
        ep.write_all(&request(0, &[])).unwrap();
        assert_eq!(take_response(&input).payload, [1]);
    }
}
