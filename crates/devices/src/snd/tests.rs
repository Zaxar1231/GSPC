use std::sync::{Arc, Mutex};

use rustdroid_virtio::mem::{GuestMemory, GuestMemoryExt, VecMemory};
use rustdroid_virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};

use super::proto::*;
use super::*;

/// Простейший драйвер: одна очередь, цепочки кладутся подряд.
struct Drv {
    mem: VecMemory,
    q: Queue,
    next_desc: u16,
    avail: u16,
}

const DESC: u64 = 0x1000;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;

impl Drv {
    fn new() -> Self {
        let mut q = Queue::new(64);
        q.desc_table = DESC;
        q.avail_ring = AVAIL;
        q.used_ring = USED;
        q.ready = true;
        Self {
            mem: VecMemory::new(0x40000),
            q,
            next_desc: 0,
            avail: 0,
        }
    }

    /// Кладёт цепочку: `(адрес, длина, для записи устройством)`.
    fn submit(&mut self, bufs: &[(u64, u32, bool)]) {
        let head = self.next_desc;
        for (i, &(addr, len, write)) in bufs.iter().enumerate() {
            let idx = self.next_desc;
            let last = i + 1 == bufs.len();
            let mut flags = if write { VIRTQ_DESC_F_WRITE } else { 0 };
            if !last {
                flags |= VIRTQ_DESC_F_NEXT;
            }
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&addr.to_le_bytes());
            b[8..12].copy_from_slice(&len.to_le_bytes());
            b[12..14].copy_from_slice(&flags.to_le_bytes());
            b[14..16].copy_from_slice(&(idx + 1).to_le_bytes());
            self.mem.write(DESC + u64::from(idx) * 16, &b).unwrap();
            self.next_desc += 1;
        }
        let slot = u64::from(self.avail % 64);
        self.mem.write_u16(AVAIL + 4 + slot * 2, head).unwrap();
        self.avail += 1;
        self.mem.write_u16(AVAIL + 2, self.avail).unwrap();
    }

    /// Длина последнего used-элемента.
    fn used_len(&self, n: u16) -> u32 {
        let slot = u64::from((n - 1) % 64);
        self.mem.read_u32(USED + 4 + slot * 8 + 4).unwrap()
    }

    fn used_idx(&self) -> u16 {
        let mut b = [0u8; 2];
        self.mem.read(USED + 2, &mut b).unwrap();
        u16::from_le_bytes(b)
    }
}

#[derive(Default)]
struct Log {
    calls: Vec<&'static str>,
    pcm: Vec<u8>,
    room: usize,
}

#[derive(Clone)]
struct Rec(Arc<Mutex<Log>>);

impl Rec {
    fn new(room: usize) -> Self {
        Self(Arc::new(Mutex::new(Log {
            room,
            ..Log::default()
        })))
    }
}

impl PcmSink for Rec {
    fn supports(&self, _p: &PcmParams) -> bool {
        true
    }
    fn prepare(&mut self, _p: &PcmParams) {
        self.0.lock().unwrap().calls.push("prepare");
    }
    fn start(&mut self) {
        self.0.lock().unwrap().calls.push("start");
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().calls.push("stop");
    }
    fn release(&mut self) {
        self.0.lock().unwrap().calls.push("release");
    }
    fn room(&self) -> usize {
        self.0.lock().unwrap().room
    }
    fn write(&mut self, pcm: &[u8]) -> u32 {
        self.0.lock().unwrap().pcm.extend_from_slice(pcm);
        64
    }
}

/// Отправляет управляющий запрос и возвращает ответ.
fn ctl(dev: &mut VirtioSnd, d: &mut Drv, req: &[u8], resp_len: u32) -> Vec<u8> {
    d.mem.write(0x10000, req).unwrap();
    d.submit(&[
        (0x10000, req.len() as u32, false),
        (0x11000, resp_len, true),
    ]);
    assert!(dev.process_queue(VQ_CONTROL, &mut d.q, &d.mem).unwrap());
    let n = d.used_len(d.used_idx());
    let mut out = vec![0u8; n as usize];
    d.mem.read(0x11000, &mut out).unwrap();
    out
}

fn status(resp: &[u8]) -> u32 {
    u32::from_le_bytes(resp[..4].try_into().unwrap())
}

fn query(code: u32, start: u32, count: u32, size: u32) -> Vec<u8> {
    [code, start, count, size]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

fn pcm(code: u32) -> Vec<u8> {
    [code, 0].iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn set_params(format: u8, rate: u8, channels: u8) -> Vec<u8> {
    let mut b: Vec<u8> = [R_PCM_SET_PARAMS, 0, 16384, 4096, 0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    b.extend_from_slice(&[channels, format, rate, 0]);
    b
}

#[test]
fn config_reports_one_stream_and_chmap() {
    let dev = VirtioSnd::new(Box::new(NullSink));
    let mut cfg = [0xffu8; 16];
    dev.read_config(0, &mut cfg);
    let word = |i: usize| u32::from_le_bytes(cfg[i * 4..i * 4 + 4].try_into().unwrap());
    assert_eq!((word(0), word(1), word(2), word(3)), (0, 1, 1, 0));
    let mut tail = [0xffu8; 4];
    dev.read_config(4, &mut tail);
    assert_eq!(u32::from_le_bytes(tail), 1);
    assert_eq!(dev.device_type(), 25);
}

#[test]
fn pcm_info_describes_output_stream() {
    let mut dev = VirtioSnd::new(Box::new(NullSink));
    let mut d = Drv::new();
    let r = ctl(&mut dev, &mut d, &query(R_PCM_INFO, 0, 1, 32), 64);
    assert_eq!(status(&r), S_OK);
    assert_eq!(r.len(), 4 + PCM_INFO_SIZE);
    let info = &r[4..];
    let formats = u64::from_le_bytes(info[8..16].try_into().unwrap());
    let rates = u64::from_le_bytes(info[16..24].try_into().unwrap());
    assert_eq!(formats, 1 << FMT_S16);
    assert_eq!(rates, (1 << RATE_44100) | (1 << RATE_48000));
    assert_eq!((info[24], info[25], info[26]), (D_OUTPUT, 1, 2));

    // Запись больше структуры дополняется нулями, меньше — ошибка.
    let r = ctl(&mut dev, &mut d, &query(R_PCM_INFO, 0, 1, 40), 64);
    assert_eq!((status(&r), r.len()), (S_OK, 44));
    let r = ctl(&mut dev, &mut d, &query(R_PCM_INFO, 0, 1, 16), 64);
    assert_eq!(status(&r), S_BAD_MSG);
}

#[test]
fn query_out_of_range_and_jacks() {
    let mut dev = VirtioSnd::new(Box::new(NullSink));
    let mut d = Drv::new();
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &query(R_PCM_INFO, 1, 1, 32), 64)),
        S_BAD_MSG
    );
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &query(R_PCM_INFO, 0, 2, 32), 64)),
        S_BAD_MSG
    );
    // Гнёзд нет: пустой запрос — успех, любой элемент — ошибка.
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &query(R_JACK_INFO, 0, 0, 24), 64)),
        S_OK
    );
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &query(R_JACK_INFO, 0, 1, 24), 64)),
        S_BAD_MSG
    );
    let r = ctl(&mut dev, &mut d, &query(R_CHMAP_INFO, 0, 1, 24), 64);
    assert_eq!(status(&r), S_OK);
    assert_eq!(&r[4 + 4..4 + 8], &[D_OUTPUT, 2, CHMAP_FL, CHMAP_FR]);
    // Неизвестный запрос и обрезанное сообщение.
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &0x0300u32.to_le_bytes(), 8)),
        S_NOT_SUPP
    );
    assert_eq!(status(&ctl(&mut dev, &mut d, &[1, 0], 8)), S_BAD_MSG);
}

#[test]
fn stream_state_machine() {
    let rec = Rec::new(usize::MAX);
    let mut dev = VirtioSnd::new(Box::new(rec.clone()));
    let mut d = Drv::new();
    // Без параметров нельзя ни подготовить, ни запустить.
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &pcm(R_PCM_START), 8)),
        S_BAD_MSG
    );
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &pcm(R_PCM_PREPARE), 8)),
        S_BAD_MSG
    );
    // Неподдерживаемые формат, частота, число каналов.
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &set_params(19, RATE_48000, 2), 8)),
        S_NOT_SUPP
    );
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &set_params(FMT_S16, 1, 2), 8)),
        S_NOT_SUPP
    );
    assert_eq!(
        status(&ctl(
            &mut dev,
            &mut d,
            &set_params(FMT_S16, RATE_48000, 6),
            8
        )),
        S_NOT_SUPP
    );
    assert_eq!(dev.phase(), Phase::Initial);

    for (req, phase) in [
        (set_params(FMT_S16, RATE_48000, 2), Phase::Params),
        (pcm(R_PCM_PREPARE), Phase::Prepared),
        (pcm(R_PCM_START), Phase::Started),
        (pcm(R_PCM_STOP), Phase::Stopped),
        (pcm(R_PCM_START), Phase::Started),
        (pcm(R_PCM_STOP), Phase::Stopped),
        (pcm(R_PCM_RELEASE), Phase::Released),
        (pcm(R_PCM_PREPARE), Phase::Prepared),
    ] {
        assert_eq!(status(&ctl(&mut dev, &mut d, &req, 8)), S_OK, "{phase:?}");
        assert_eq!(dev.phase(), phase);
    }
    // STOP без START и чужой поток — ошибка, состояние не меняется.
    assert_eq!(
        status(&ctl(&mut dev, &mut d, &pcm(R_PCM_STOP), 8)),
        S_BAD_MSG
    );
    let mut other = pcm(R_PCM_START);
    other[4] = 1;
    assert_eq!(status(&ctl(&mut dev, &mut d, &other, 8)), S_BAD_MSG);
    assert_eq!(dev.phase(), Phase::Prepared);
    assert_eq!(
        rec.0.lock().unwrap().calls,
        [
            "prepare", "start", "stop", "start", "stop", "release", "prepare"
        ]
    );
}

#[test]
fn tx_delivers_pcm_and_reports_status() {
    let rec = Rec::new(usize::MAX);
    let mut dev = VirtioSnd::new(Box::new(rec.clone()));
    let mut c = Drv::new();
    for req in [
        set_params(FMT_S16, RATE_48000, 2),
        pcm(R_PCM_PREPARE),
        pcm(R_PCM_START),
    ] {
        assert_eq!(status(&ctl(&mut dev, &mut c, &req, 8)), S_OK);
    }
    // Заголовок и отсчёты — в разных дескрипторах, статус — в последнем.
    let mut t = Drv::new();
    t.mem.write(0x10000, &0u32.to_le_bytes()).unwrap();
    t.mem.write(0x10100, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    t.submit(&[(0x10000, 4, false), (0x10100, 8, false), (0x11000, 8, true)]);
    assert!(dev.process_queue(VQ_TX, &mut t.q, &t.mem).unwrap());
    assert_eq!(t.used_len(1), 8);
    let mut st = [0u8; 8];
    t.mem.read(0x11000, &mut st).unwrap();
    assert_eq!(u32::from_le_bytes(st[..4].try_into().unwrap()), S_OK);
    assert_eq!(u32::from_le_bytes(st[4..].try_into().unwrap()), 64);
    assert_eq!(rec.0.lock().unwrap().pcm, [1, 2, 3, 4, 5, 6, 7, 8]);

    // Чужой поток — BAD_MSG, данные не уходят.
    t.mem.write(0x12000, &7u32.to_le_bytes()).unwrap();
    t.submit(&[(0x12000, 4, false), (0x13000, 8, true)]);
    assert!(dev.process_queue(VQ_TX, &mut t.q, &t.mem).unwrap());
    t.mem.read(0x13000, &mut st).unwrap();
    assert_eq!(u32::from_le_bytes(st[..4].try_into().unwrap()), S_BAD_MSG);
    assert_eq!(rec.0.lock().unwrap().pcm.len(), 8);
}

#[test]
fn tx_waits_when_sink_is_full_and_rejects_before_prepare() {
    let rec = Rec::new(0);
    let mut dev = VirtioSnd::new(Box::new(rec.clone()));
    let mut t = Drv::new();
    t.mem.write(0x10000, &0u32.to_le_bytes()).unwrap();
    t.submit(&[(0x10000, 4, false), (0x11000, 8, true)]);
    // Места нет — буфер остаётся у гостя.
    assert!(!dev.process_queue(VQ_TX, &mut t.q, &t.mem).unwrap());
    assert_eq!(t.used_idx(), 0);
    // Место появилось, но поток не подготовлен — IO_ERR.
    rec.0.lock().unwrap().room = 4096;
    assert!(dev.process_queue(VQ_TX, &mut t.q, &t.mem).unwrap());
    let mut st = [0u8; 4];
    t.mem.read(0x11000, &mut st).unwrap();
    assert_eq!(u32::from_le_bytes(st), S_IO_ERR);
}

#[test]
fn reset_releases_running_stream() {
    let rec = Rec::new(usize::MAX);
    let mut dev = VirtioSnd::new(Box::new(rec.clone()));
    let mut d = Drv::new();
    for req in [
        set_params(FMT_S16, RATE_44100, 1),
        pcm(R_PCM_PREPARE),
        pcm(R_PCM_START),
    ] {
        assert_eq!(status(&ctl(&mut dev, &mut d, &req, 8)), S_OK);
    }
    dev.reset();
    assert_eq!(dev.phase(), Phase::Initial);
    assert_eq!(rec.0.lock().unwrap().calls.last(), Some(&"release"));
}

#[test]
fn proto_parse_and_rates() {
    let b = set_params(FMT_S16, RATE_48000, 2);
    assert_eq!(
        Request::parse(&b),
        Some(Request::SetParams {
            stream: 0,
            params: PcmParams {
                buffer_bytes: 16384,
                period_bytes: 4096,
                features: 0,
                channels: 2,
                format: FMT_S16,
                rate: RATE_48000,
            }
        })
    );
    assert_eq!(Request::parse(&b[..20]), None);
    assert_eq!(rate_hz(RATE_44100), Some(44_100));
    assert_eq!(rate_hz(RATE_48000), Some(48_000));
    assert_eq!(rate_hz(14), None);
}
