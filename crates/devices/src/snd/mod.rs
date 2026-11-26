//! virtio-snd: один поток вывода (стерео, S16, 44,1/48 кГц) поверх [`PcmSink`].
//!
//! Управляющая очередь отвечает на `*_INFO` и ведёт машину состояний потока
//! (virtio 1.2, 5.14.6.6.1): `SET_PARAMS` → `PREPARE` → `START` ⇄ `STOP` → `RELEASE`.
//! Очередь вывода передаёт отсчёты в [`PcmSink`]; буферы гостя забираются, только пока у
//! вывода есть место, иначе ждут в очереди (как приём у virtio-console).
//!
//! Ограничения (этап 9.2): ввода (микрофона), гнёзд и элементов управления нет; очередь
//! событий не используется. [`NullSink`] отбрасывает звук и принимает его сразу, без темпа
//! реального времени — настоящий вывод (WASAPI, 9.3) должен сам ограничивать `room` и будить
//! устройство, когда место освободилось.

pub mod proto;

#[cfg(test)]
mod tests;

use rustdroid_virtio::device::{TYPE_SOUND, VirtioDevice};
use rustdroid_virtio::mem::GuestMemory;
use rustdroid_virtio::queue::{Chain, Queue, QueueError};

use proto::{
    CHMAP_FL, CHMAP_FR, CHMAP_INFO_SIZE, D_OUTPUT, FMT_S16, JACK_INFO_SIZE, PCM_INFO_SIZE,
    PCM_STATUS_SIZE, PcmParams, R_CHMAP_INFO, R_JACK_INFO, R_PCM_INFO, R_PCM_PREPARE,
    R_PCM_RELEASE, R_PCM_START, R_PCM_STOP, RATE_44100, RATE_48000, Request, S_BAD_MSG, S_IO_ERR,
    S_NOT_SUPP, S_OK, StreamInfo,
};

/// Вывод звука на хосте.
pub trait PcmSink: Send {
    /// Поддерживает ли вывод такие параметры (формат и частота уже проверены устройством).
    fn supports(&self, params: &PcmParams) -> bool;
    /// Поток подготовлен с параметрами.
    fn prepare(&mut self, params: &PcmParams);
    /// Воспроизведение началось.
    fn start(&mut self);
    /// Воспроизведение остановлено.
    fn stop(&mut self);
    /// Поток освобождён.
    fn release(&mut self);
    /// Сколько байт вывод готов принять сейчас; 0 — буферы гостя подождут.
    fn room(&self) -> usize;
    /// Отсчёты гостя; возвращает задержку вывода в байтах.
    fn write(&mut self, pcm: &[u8]) -> u32;
}

/// Вывод, который отбрасывает звук (пока нет WASAPI).
#[derive(Debug, Default)]
pub struct NullSink;

impl PcmSink for NullSink {
    fn supports(&self, _params: &PcmParams) -> bool {
        true
    }
    fn prepare(&mut self, _params: &PcmParams) {}
    fn start(&mut self) {}
    fn stop(&mut self) {}
    fn release(&mut self) {}
    fn room(&self) -> usize {
        usize::MAX
    }
    fn write(&mut self, _pcm: &[u8]) -> u32 {
        0
    }
}

/// Состояние потока.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// После сброса: параметров нет.
    Initial,
    /// Параметры заданы.
    Params,
    /// Подготовлен.
    Prepared,
    /// Играет.
    Started,
    /// Остановлен.
    Stopped,
    /// Освобождён (параметры сохранены).
    Released,
}

impl Phase {
    /// Следующее состояние по запросу `code` или `None`, если переход запрещён.
    #[must_use]
    pub fn after(self, code: u32) -> Option<Self> {
        use Phase::{Initial, Params, Prepared, Released, Started, Stopped};
        match (code, self) {
            (proto::R_PCM_SET_PARAMS, Initial | Params | Prepared | Released) => Some(Params),
            (R_PCM_PREPARE, Params | Prepared | Released) => Some(Prepared),
            (R_PCM_START, Prepared | Stopped) => Some(Started),
            (R_PCM_STOP, Started) => Some(Stopped),
            (R_PCM_RELEASE, Prepared | Stopped) => Some(Released),
            _ => None,
        }
    }
}

/// Поток вывода, который отдаёт устройство.
const OUTPUT: StreamInfo = StreamInfo {
    direction: D_OUTPUT,
    channels_min: 1,
    channels_max: 2,
    formats: 1 << FMT_S16,
    rates: (1 << RATE_44100) | (1 << RATE_48000),
};

/// virtio-snd.
pub struct VirtioSnd {
    sink: Box<dyn PcmSink>,
    phase: Phase,
    params: Option<PcmParams>,
    sizes: [u16; 4],
    chain: Chain,
    req: Vec<u8>,
}

impl std::fmt::Debug for VirtioSnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioSnd")
            .field("phase", &self.phase)
            .field("params", &self.params)
            .finish_non_exhaustive()
    }
}

impl VirtioSnd {
    /// Устройство с выводом в `sink`.
    #[must_use]
    pub fn new(sink: Box<dyn PcmSink>) -> Self {
        Self {
            sink,
            phase: Phase::Initial,
            params: None,
            sizes: [64; 4],
            chain: Chain::default(),
            req: Vec::new(),
        }
    }

    /// Текущее состояние потока.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Читает все байты, которые гость дал устройству, в `self.req`.
    fn read_request(&mut self, mem: &dyn GuestMemory) -> bool {
        self.req.clear();
        for d in self.chain.descriptors.iter().filter(|d| !d.is_write()) {
            let start = self.req.len();
            self.req.resize(start + d.len as usize, 0);
            if mem.read(d.addr, &mut self.req[start..]).is_err() {
                return false;
            }
        }
        true
    }

    /// Пишет `resp` в буферы для записи устройством; возвращает записанное число байт.
    fn write_response(&self, mem: &dyn GuestMemory, resp: &[u8]) -> u32 {
        let mut done = 0usize;
        for d in self.chain.descriptors.iter().filter(|d| d.is_write()) {
            if done >= resp.len() {
                break;
            }
            let n = (d.len as usize).min(resp.len() - done);
            if mem.write(d.addr, &resp[done..done + n]).is_err() {
                break;
            }
            done += n;
        }
        u32::try_from(done).unwrap_or(u32::MAX)
    }

    /// Ответ на запрос управляющей очереди.
    fn control(&mut self, req: Option<Request>) -> Vec<u8> {
        let status = |s: u32| s.to_le_bytes().to_vec();
        let Some(req) = req else {
            return status(S_BAD_MSG);
        };
        match req {
            Request::Query {
                code,
                start_id,
                count,
                size,
            } => {
                let (items, item_size): (Vec<Vec<u8>>, usize) = match code {
                    R_PCM_INFO => (vec![OUTPUT.encode().to_vec()], PCM_INFO_SIZE),
                    R_CHMAP_INFO => (
                        vec![proto::encode_chmap(D_OUTPUT, &[CHMAP_FL, CHMAP_FR]).to_vec()],
                        CHMAP_INFO_SIZE,
                    ),
                    R_JACK_INFO => (Vec::new(), JACK_INFO_SIZE),
                    _ => return status(S_NOT_SUPP),
                };
                let (start, count, size) = (start_id as usize, count as usize, size as usize);
                let end = start.checked_add(count);
                if size < item_size || end.is_none_or(|e| e > items.len()) {
                    return status(S_BAD_MSG);
                }
                let mut out = status(S_OK);
                for item in &items[start..start + count] {
                    let mut b = item.clone();
                    b.resize(size, 0);
                    out.extend_from_slice(&b);
                }
                out
            }
            Request::SetParams { stream, params } => {
                if stream != 0 {
                    return status(S_BAD_MSG);
                }
                let Some(next) = self.phase.after(proto::R_PCM_SET_PARAMS) else {
                    return status(S_BAD_MSG);
                };
                if params.period_bytes == 0 || params.period_bytes > params.buffer_bytes {
                    return status(S_BAD_MSG);
                }
                let format_ok = OUTPUT.formats & (1u64 << params.format.min(63)) != 0;
                let rate_ok = OUTPUT.rates & (1u64 << params.rate.min(63)) != 0;
                let channels_ok =
                    (OUTPUT.channels_min..=OUTPUT.channels_max).contains(&params.channels);
                if !(format_ok && rate_ok && channels_ok && self.sink.supports(&params)) {
                    return status(S_NOT_SUPP);
                }
                self.params = Some(params);
                self.phase = next;
                status(S_OK)
            }
            Request::Pcm { code, stream } => {
                if stream != 0 {
                    return status(S_BAD_MSG);
                }
                let Some(next) = self.phase.after(code) else {
                    return status(S_BAD_MSG);
                };
                match code {
                    R_PCM_PREPARE => match self.params {
                        Some(p) => self.sink.prepare(&p),
                        None => return status(S_BAD_MSG),
                    },
                    R_PCM_START => self.sink.start(),
                    R_PCM_STOP => self.sink.stop(),
                    R_PCM_RELEASE => self.sink.release(),
                    _ => {}
                }
                self.phase = next;
                status(S_OK)
            }
            Request::Other(_) => status(S_NOT_SUPP),
        }
    }

    fn process_control(
        &mut self,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        let mut any = false;
        while queue.pop(mem, &mut self.chain)? {
            let req = if self.read_request(mem) {
                Request::parse(&self.req)
            } else {
                None
            };
            let resp = self.control(req);
            let len = self.write_response(mem, &resp);
            queue.add_used(mem, self.chain.head, len)?;
            any = true;
        }
        Ok(any)
    }

    fn process_tx(&mut self, queue: &mut Queue, mem: &dyn GuestMemory) -> Result<bool, QueueError> {
        let mut any = false;
        while self.sink.room() > 0 && queue.pop(mem, &mut self.chain)? {
            let ok = self.read_request(mem);
            let stream = self
                .req
                .get(..4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map(u32::from_le_bytes);
            let playing = matches!(self.phase, Phase::Prepared | Phase::Started);
            let (status, latency) = match stream {
                _ if !ok => (S_IO_ERR, 0),
                Some(0) if playing => {
                    let latency = self.sink.write(self.req.get(4..).unwrap_or(&[]));
                    (S_OK, latency)
                }
                Some(0) => (S_IO_ERR, 0),
                _ => (S_BAD_MSG, 0),
            };
            let resp = proto::encode_status(status, latency);
            let len = self.write_response(mem, &resp);
            debug_assert!(len as usize <= PCM_STATUS_SIZE);
            queue.add_used(mem, self.chain.head, len)?;
            any = true;
        }
        Ok(any)
    }
}

impl VirtioDevice for VirtioSnd {
    fn device_type(&self) -> u32 {
        TYPE_SOUND
    }

    fn device_features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // jacks, streams, chmaps, controls.
        let mut cfg = [0u8; 16];
        cfg[4..8].copy_from_slice(&1u32.to_le_bytes());
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes());
        data.fill(0);
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        if let Some(src) = cfg.get(off..) {
            let n = src.len().min(data.len());
            data[..n].copy_from_slice(&src[..n]);
        }
    }

    fn reset(&mut self) {
        if !matches!(self.phase, Phase::Initial | Phase::Released) {
            self.sink.release();
        }
        self.phase = Phase::Initial;
        self.params = None;
    }

    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        match index {
            proto::VQ_CONTROL => self.process_control(queue, mem),
            proto::VQ_TX => self.process_tx(queue, mem),
            // Событий не шлём, потоков ввода нет: буферы гостя остаются в очередях.
            _ => Ok(false),
        }
    }
}
