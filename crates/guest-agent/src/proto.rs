//! Сообщения агента и их кодирование.

/// Порт vsock хоста, на который подключается агент.
pub const PORT: u32 = 40_000;

/// Версия протокола.
pub const VERSION: u32 = 1;

/// Самое длинное допустимое сообщение (тип и тело), байт. Данные установки идут кусками
/// меньше этого предела.
pub const MAX_MESSAGE: usize = 1 << 20;

/// Размер куска данных установки, байт.
pub const CHUNK: usize = 32 * 1024;

/// Сообщение протокола.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Агент → хост: агент запущен.
    Hello {
        /// Версия протокола агента.
        version: u32,
    },
    /// Хост → агент: хост принял соединение.
    HelloAck {
        /// Версия протокола хоста.
        version: u32,
    },
    /// Агент → хост: на экране приложение `package` (пустая строка — главный экран или нет
    /// приложения).
    Foreground {
        /// Имя пакета.
        package: String,
    },
    /// Хост → агент: начать приём APK `id` размером `size` байт.
    InstallBegin {
        /// Идентификатор установки.
        id: u32,
        /// Размер файла.
        size: u64,
        /// Имя файла (для сообщений).
        name: String,
    },
    /// Хост → агент: очередной кусок APK.
    InstallChunk {
        /// Идентификатор установки.
        id: u32,
        /// Данные.
        data: Vec<u8>,
    },
    /// Хост → агент: данные закончились, установить.
    InstallEnd {
        /// Идентификатор установки.
        id: u32,
    },
    /// Агент → хост: итог установки.
    InstallResult {
        /// Идентификатор установки.
        id: u32,
        /// Установка удалась.
        ok: bool,
        /// Вывод `pm install` или причина отказа.
        message: String,
    },
    /// Любая сторона: проверка связи.
    Ping {
        /// Значение, которое вернётся в [`Message::Pong`].
        token: u32,
    },
    /// Ответ на [`Message::Ping`].
    Pong {
        /// Значение из запроса.
        token: u32,
    },
}

const T_HELLO: u8 = 1;
const T_HELLO_ACK: u8 = 2;
const T_FOREGROUND: u8 = 3;
const T_INSTALL_BEGIN: u8 = 4;
const T_INSTALL_CHUNK: u8 = 5;
const T_INSTALL_END: u8 = 6;
const T_INSTALL_RESULT: u8 = 7;
const T_PING: u8 = 8;
const T_PONG: u8 = 9;

fn put_str(out: &mut Vec<u8>, s: &str) {
    // Строки короче 64 КиБ: длинные обрезаются по границе символа.
    let mut end = s.len().min(usize::from(u16::MAX));
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    #[allow(clippy::cast_possible_truncation, reason = "end ≤ u16::MAX")]
    out.extend_from_slice(&(end as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes().get(..end).unwrap_or(&[]));
}

/// Кодирует сообщение в кадр.
#[must_use]
pub fn encode(m: &Message) -> Vec<u8> {
    let mut body = Vec::new();
    let ty = match m {
        Message::Hello { version } => {
            body.extend_from_slice(&version.to_le_bytes());
            T_HELLO
        }
        Message::HelloAck { version } => {
            body.extend_from_slice(&version.to_le_bytes());
            T_HELLO_ACK
        }
        Message::Foreground { package } => {
            put_str(&mut body, package);
            T_FOREGROUND
        }
        Message::InstallBegin { id, size, name } => {
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(&size.to_le_bytes());
            put_str(&mut body, name);
            T_INSTALL_BEGIN
        }
        Message::InstallChunk { id, data } => {
            body.extend_from_slice(&id.to_le_bytes());
            body.extend_from_slice(data);
            T_INSTALL_CHUNK
        }
        Message::InstallEnd { id } => {
            body.extend_from_slice(&id.to_le_bytes());
            T_INSTALL_END
        }
        Message::InstallResult { id, ok, message } => {
            body.extend_from_slice(&id.to_le_bytes());
            body.push(u8::from(*ok));
            put_str(&mut body, message);
            T_INSTALL_RESULT
        }
        Message::Ping { token } => {
            body.extend_from_slice(&token.to_le_bytes());
            T_PING
        }
        Message::Pong { token } => {
            body.extend_from_slice(&token.to_le_bytes());
            T_PONG
        }
    };
    #[allow(clippy::cast_possible_truncation, reason = "кадры короче MAX_MESSAGE")]
    let len = (1 + body.len()) as u32;
    let mut out = Vec::with_capacity(4 + 1 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.push(ty);
    out.extend_from_slice(&body);
    out
}

/// Ошибка разбора потока: дальше по нему читать нельзя (соединение надо закрыть).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamError;

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("повреждённый поток сообщений агента")
    }
}

impl std::error::Error for StreamError {}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }
    fn str(&mut self) -> Option<String> {
        let len = usize::from(u16::from_le_bytes(self.take(2)?.try_into().ok()?));
        Some(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }
}

fn decode(ty: u8, body: &[u8]) -> Option<Message> {
    let mut r = Reader(body);
    Some(match ty {
        T_HELLO => Message::Hello { version: r.u32()? },
        T_HELLO_ACK => Message::HelloAck { version: r.u32()? },
        T_FOREGROUND => Message::Foreground { package: r.str()? },
        T_INSTALL_BEGIN => Message::InstallBegin {
            id: r.u32()?,
            size: r.u64()?,
            name: r.str()?,
        },
        T_INSTALL_CHUNK => Message::InstallChunk {
            id: r.u32()?,
            data: r.rest().to_vec(),
        },
        T_INSTALL_END => Message::InstallEnd { id: r.u32()? },
        T_INSTALL_RESULT => Message::InstallResult {
            id: r.u32()?,
            ok: r.u8()? != 0,
            message: r.str()?,
        },
        T_PING => Message::Ping { token: r.u32()? },
        T_PONG => Message::Pong { token: r.u32()? },
        _ => return None,
    })
}

/// Накапливает байты потока и выдаёт готовые сообщения.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    /// Добавляет данные; возвращает все полностью пришедшие сообщения. Неизвестный тип
    /// пропускается (протокол расширяемый), но неверная длина — [`StreamError`].
    ///
    /// # Errors
    /// Длина сообщения нулевая или больше [`MAX_MESSAGE`], либо тело известного типа неверно.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Message>, StreamError> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut at = 0usize;
        while let Some(head) = self.buf.get(at..at + 4) {
            let len = u32::from_le_bytes(head.try_into().map_err(|_| StreamError)?) as usize;
            if len == 0 || len > MAX_MESSAGE {
                return Err(StreamError);
            }
            let Some(frame) = self.buf.get(at + 4..at + 4 + len) else {
                break;
            };
            let (&ty, body) = frame.split_first().ok_or(StreamError)?;
            if (T_HELLO..=T_PONG).contains(&ty) {
                out.push(decode(ty, body).ok_or(StreamError)?);
            }
            at += 4 + len;
        }
        self.buf.drain(..at);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        vec![
            Message::Hello { version: VERSION },
            Message::HelloAck { version: VERSION },
            Message::Foreground {
                package: "com.example.игра".into(),
            },
            Message::Foreground {
                package: String::new(),
            },
            Message::InstallBegin {
                id: 7,
                size: 5_000_000_000,
                name: "app.apk".into(),
            },
            Message::InstallChunk {
                id: 7,
                data: (0..=255).collect(),
            },
            Message::InstallEnd { id: 7 },
            Message::InstallResult {
                id: 7,
                ok: false,
                message: "Failure [INSTALL_FAILED_OLDER_SDK]".into(),
            },
            Message::Ping { token: 1 },
            Message::Pong { token: 2 },
        ]
    }

    #[test]
    fn every_message_roundtrips() {
        for m in samples() {
            let mut d = Decoder::default();
            assert_eq!(d.feed(&encode(&m)).unwrap(), vec![m]);
        }
    }

    #[test]
    fn stream_split_at_every_byte() {
        let all: Vec<u8> = samples().iter().flat_map(encode).collect();
        let mut d = Decoder::default();
        let mut got = Vec::new();
        for b in &all {
            got.extend(d.feed(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got, samples());
    }

    #[test]
    fn unknown_type_is_skipped_bad_length_is_error() {
        let mut d = Decoder::default();
        // Тип 200 пропускается, следующее сообщение читается.
        let mut s = vec![3, 0, 0, 0, 200, 1, 2];
        s.extend(encode(&Message::Ping { token: 9 }));
        assert_eq!(d.feed(&s).unwrap(), vec![Message::Ping { token: 9 }]);
        assert_eq!(Decoder::default().feed(&[0, 0, 0, 0]), Err(StreamError));
        let huge = u32::try_from(MAX_MESSAGE + 1).unwrap().to_le_bytes();
        assert_eq!(Decoder::default().feed(&huge), Err(StreamError));
        // Тело известного типа короче положенного.
        assert_eq!(
            Decoder::default().feed(&[2, 0, 0, 0, T_HELLO, 1]),
            Err(StreamError)
        );
    }

    #[test]
    fn long_strings_are_cut_at_char_boundary() {
        let long = "я".repeat(40_000);
        let m = Message::Foreground { package: long };
        let got = Decoder::default().feed(&encode(&m)).unwrap();
        let Some(Message::Foreground { package }) = got.first() else {
            panic!("нет сообщения");
        };
        assert!(package.len() <= usize::from(u16::MAX));
        assert!(package.chars().all(|c| c == 'я'));
    }
}
