//! Формат сообщений virtio-snd (virtio 1.2, раздел 5.14; `include/uapi/linux/virtio_snd.h`).
//!
//! Все поля — little-endian. Здесь только разбор и сборка байт, без очередей.

/// Очереди устройства.
pub const VQ_CONTROL: usize = 0;
/// Очередь событий.
pub const VQ_EVENT: usize = 1;
/// Вывод звука (гость → хост).
pub const VQ_TX: usize = 2;
/// Запись звука (хост → гость).
pub const VQ_RX: usize = 3;

/// Направление потока: вывод.
pub const D_OUTPUT: u8 = 0;
/// Направление потока: ввод.
pub const D_INPUT: u8 = 1;

/// Запросы управляющей очереди.
pub const R_JACK_INFO: u32 = 1;
/// Переназначение гнезда.
pub const R_JACK_REMAP: u32 = 2;
/// Сведения о потоках PCM.
pub const R_PCM_INFO: u32 = 0x0100;
/// Параметры потока.
pub const R_PCM_SET_PARAMS: u32 = 0x0101;
/// Подготовка потока.
pub const R_PCM_PREPARE: u32 = 0x0102;
/// Освобождение потока.
pub const R_PCM_RELEASE: u32 = 0x0103;
/// Запуск потока.
pub const R_PCM_START: u32 = 0x0104;
/// Остановка потока.
pub const R_PCM_STOP: u32 = 0x0105;
/// Сведения о раскладках каналов.
pub const R_CHMAP_INFO: u32 = 0x0200;

/// Статусы ответа.
pub const S_OK: u32 = 0x8000;
/// Неверное сообщение.
pub const S_BAD_MSG: u32 = 0x8001;
/// Не поддерживается.
pub const S_NOT_SUPP: u32 = 0x8002;
/// Ошибка ввода-вывода.
pub const S_IO_ERR: u32 = 0x8003;

/// Формат отсчётов S16 (`VIRTIO_SND_PCM_FMT_S16`).
pub const FMT_S16: u8 = 5;
/// Частота 44100 Гц (`VIRTIO_SND_PCM_RATE_44100`).
pub const RATE_44100: u8 = 6;
/// Частота 48000 Гц.
pub const RATE_48000: u8 = 7;

/// Позиции каналов: левый передний и правый передний.
pub const CHMAP_FL: u8 = 3;
/// Правый передний.
pub const CHMAP_FR: u8 = 4;

/// Размер `virtio_snd_pcm_info`.
pub const PCM_INFO_SIZE: usize = 32;
/// Размер `virtio_snd_chmap_info`.
pub const CHMAP_INFO_SIZE: usize = 24;
/// Размер `virtio_snd_jack_info`.
pub const JACK_INFO_SIZE: usize = 24;
/// Число позиций в `virtio_snd_chmap_info`.
pub const CHMAP_MAX_SIZE: usize = 18;
/// Размер `virtio_snd_pcm_status`.
pub const PCM_STATUS_SIZE: usize = 8;

/// Частота в герцах для кода `VIRTIO_SND_PCM_RATE_*`.
#[must_use]
pub fn rate_hz(code: u8) -> Option<u32> {
    const RATES: [u32; 14] = [
        5512, 8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176_400,
        192_000, 384_000,
    ];
    RATES.get(usize::from(code)).copied()
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(s))
}

/// Параметры потока из `PCM_SET_PARAMS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmParams {
    /// Размер буфера, байт.
    pub buffer_bytes: u32,
    /// Размер периода, байт.
    pub period_bytes: u32,
    /// Выбранные возможности.
    pub features: u32,
    /// Каналов.
    pub channels: u8,
    /// Формат (`VIRTIO_SND_PCM_FMT_*`).
    pub format: u8,
    /// Частота (`VIRTIO_SND_PCM_RATE_*`).
    pub rate: u8,
}

/// Запрос управляющей очереди.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// `*_INFO`: сведения о `count` элементах с `start_id`, по `size` байт.
    Query {
        /// Код запроса.
        code: u32,
        /// Первый элемент.
        start_id: u32,
        /// Сколько элементов.
        count: u32,
        /// Размер сведений об элементе.
        size: u32,
    },
    /// `PCM_SET_PARAMS`.
    SetParams {
        /// Поток.
        stream: u32,
        /// Параметры.
        params: PcmParams,
    },
    /// `PCM_PREPARE`, `PCM_RELEASE`, `PCM_START`, `PCM_STOP`.
    Pcm {
        /// Код запроса.
        code: u32,
        /// Поток.
        stream: u32,
    },
    /// Неизвестный или неподдерживаемый запрос.
    Other(u32),
}

impl Request {
    /// Разбирает запрос; `None` — сообщение короче своего заголовка.
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        let code = le32(b, 0)?;
        Some(match code {
            R_JACK_INFO | R_PCM_INFO | R_CHMAP_INFO => Self::Query {
                code,
                start_id: le32(b, 4)?,
                count: le32(b, 8)?,
                size: le32(b, 12)?,
            },
            R_PCM_SET_PARAMS => Self::SetParams {
                stream: le32(b, 4)?,
                params: PcmParams {
                    buffer_bytes: le32(b, 8)?,
                    period_bytes: le32(b, 12)?,
                    features: le32(b, 16)?,
                    channels: *b.get(20)?,
                    format: *b.get(21)?,
                    rate: *b.get(22)?,
                },
            },
            R_PCM_PREPARE | R_PCM_RELEASE | R_PCM_START | R_PCM_STOP => Self::Pcm {
                code,
                stream: le32(b, 4)?,
            },
            other => Self::Other(other),
        })
    }
}

/// Описание потока для ответа `PCM_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// Направление (`D_*`).
    pub direction: u8,
    /// Наименьшее число каналов.
    pub channels_min: u8,
    /// Наибольшее число каналов.
    pub channels_max: u8,
    /// Битовая маска форматов (`1 << FMT_*`).
    pub formats: u64,
    /// Битовая маска частот (`1 << RATE_*`).
    pub rates: u64,
}

impl StreamInfo {
    /// `virtio_snd_pcm_info`.
    #[must_use]
    pub fn encode(&self) -> [u8; PCM_INFO_SIZE] {
        let mut b = [0u8; PCM_INFO_SIZE];
        // hdr.hda_fn_nid = 0, features = 0.
        b[8..16].copy_from_slice(&self.formats.to_le_bytes());
        b[16..24].copy_from_slice(&self.rates.to_le_bytes());
        b[24] = self.direction;
        b[25] = self.channels_min;
        b[26] = self.channels_max;
        b
    }
}

/// `virtio_snd_chmap_info`.
#[must_use]
pub fn encode_chmap(direction: u8, positions: &[u8]) -> [u8; CHMAP_INFO_SIZE] {
    let mut b = [0u8; CHMAP_INFO_SIZE];
    let n = positions.len().min(CHMAP_MAX_SIZE);
    b[4] = direction;
    #[allow(clippy::cast_possible_truncation, reason = "n ≤ 18")]
    {
        b[5] = n as u8;
    }
    b[6..6 + n].copy_from_slice(&positions[..n]);
    b
}

/// `virtio_snd_pcm_status`.
#[must_use]
pub fn encode_status(status: u32, latency_bytes: u32) -> [u8; PCM_STATUS_SIZE] {
    let mut b = [0u8; PCM_STATUS_SIZE];
    b[..4].copy_from_slice(&status.to_le_bytes());
    b[4..].copy_from_slice(&latency_bytes.to_le_bytes());
    b
}
