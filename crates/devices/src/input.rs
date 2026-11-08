//! virtio-input (virtio 1.2, раздел 5.8): мультитач-экран и клавиатура.
//!
//! События evdev (`type`, `code`, `value`) копятся в [`InputEvents`] и отдаются гостю пачками
//! до `SYN_REPORT`. Хост добавляет событие и сразу будит очередь — без ожидания кадра.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use rustdroid_virtio::device::TYPE_INPUT;
use rustdroid_virtio::{Chain, GuestMemory, Queue, QueueError, VirtioDevice};

/// Типы событий evdev.
pub mod ev {
    /// Синхронизация.
    pub const SYN: u16 = 0x00;
    /// Клавиши и кнопки.
    pub const KEY: u16 = 0x01;
    /// Абсолютные оси.
    pub const ABS: u16 = 0x03;
    /// `SYN_REPORT`.
    pub const SYN_REPORT: u16 = 0;
    /// `BTN_TOUCH`.
    pub const BTN_TOUCH: u16 = 0x14A;
    /// `ABS_X`.
    pub const ABS_X: u16 = 0x00;
    /// `ABS_Y`.
    pub const ABS_Y: u16 = 0x01;
    /// `ABS_MT_SLOT`.
    pub const ABS_MT_SLOT: u16 = 0x2F;
    /// `ABS_MT_POSITION_X`.
    pub const ABS_MT_POSITION_X: u16 = 0x35;
    /// `ABS_MT_POSITION_Y`.
    pub const ABS_MT_POSITION_Y: u16 = 0x36;
    /// `ABS_MT_TRACKING_ID`.
    pub const ABS_MT_TRACKING_ID: u16 = 0x39;
    /// `INPUT_PROP_DIRECT` (сенсорный экран, а не тачпад).
    pub const INPUT_PROP_DIRECT: u16 = 0x01;
}

const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

/// Событие evdev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputEvent {
    /// Тип.
    pub kind: u16,
    /// Код.
    pub code: u16,
    /// Значение.
    pub value: i32,
}

impl InputEvent {
    fn bytes(self) -> [u8; 8] {
        let mut b = [0_u8; 8];
        b[0..2].copy_from_slice(&self.kind.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

/// Очередь событий от хоста к устройству.
#[derive(Debug, Default)]
pub struct InputEvents(Mutex<VecDeque<InputEvent>>);

impl InputEvents {
    /// Добавляет события (обычно — пачку, заканчивающуюся `SYN_REPORT`).
    pub fn push(&self, events: &[InputEvent]) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(events.iter().copied());
    }

    fn pop(&self) -> Option<InputEvent> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn is_empty(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }
}

/// Вид устройства.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// Мультитач-экран: диапазон координат и число слотов.
    Touchscreen {
        /// Ширина (ABS_MT_POSITION_X 0..width-1).
        width: u32,
        /// Высота.
        height: u32,
        /// Число одновременных касаний.
        slots: u32,
    },
    /// Клавиатура.
    Keyboard,
}

/// virtio-input.
pub struct VirtioInput {
    kind: InputKind,
    name: String,
    events: Arc<InputEvents>,
    select: u8,
    subsel: u8,
    sizes: [u16; 2],
    chain: Chain,
}

impl std::fmt::Debug for VirtioInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtioInput")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn set_bit(bits: &mut [u8; 128], n: u16) {
    if let Some(b) = bits.get_mut(usize::from(n / 8)) {
        *b |= 1 << (n % 8);
    }
}

impl VirtioInput {
    /// Устройство `kind`; события берутся из `events`.
    #[must_use]
    pub fn new(kind: InputKind, name: &str, events: Arc<InputEvents>) -> Self {
        Self {
            kind,
            name: name.to_owned(),
            events,
            select: 0,
            subsel: 0,
            sizes: [64, 8],
            chain: Chain::default(),
        }
    }

    /// Данные конфигурации для текущих `select`/`subsel`.
    fn config_data(&self) -> (u8, [u8; 128]) {
        let mut d = [0_u8; 128];
        let len = |n: usize| u8::try_from(n.min(128)).unwrap_or(128);
        match self.select {
            CFG_ID_NAME => {
                let n = self.name.len().min(128);
                d[..n].copy_from_slice(self.name.as_bytes().get(..n).unwrap_or(&[]));
                (len(n), d)
            }
            CFG_ID_SERIAL => {
                d[..9].copy_from_slice(b"rustdroid");
                (9, d)
            }
            CFG_ID_DEVIDS => {
                // bustype BUS_VIRTUAL (0x06), vendor, product, version.
                let product: u16 = match self.kind {
                    InputKind::Touchscreen { .. } => 1,
                    InputKind::Keyboard => 2,
                };
                for (i, v) in [0x06_u16, 0x1AF4, product, 1].iter().enumerate() {
                    d[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
                }
                (8, d)
            }
            CFG_PROP_BITS => match self.kind {
                InputKind::Touchscreen { .. } => {
                    set_bit(&mut d, ev::INPUT_PROP_DIRECT);
                    (1, d)
                }
                InputKind::Keyboard => (0, d),
            },
            CFG_EV_BITS => self.ev_bits(u16::from(self.subsel), d),
            CFG_ABS_INFO => match self.kind {
                InputKind::Touchscreen {
                    width,
                    height,
                    slots,
                } => {
                    let max = match u16::from(self.subsel) {
                        ev::ABS_X | ev::ABS_MT_POSITION_X => width.saturating_sub(1),
                        ev::ABS_Y | ev::ABS_MT_POSITION_Y => height.saturating_sub(1),
                        ev::ABS_MT_SLOT => slots.saturating_sub(1),
                        ev::ABS_MT_TRACKING_ID => 0xFFFF,
                        _ => return (0, d),
                    };
                    // min, max, fuzz, flat, res
                    d[4..8].copy_from_slice(&max.to_le_bytes());
                    (20, d)
                }
                InputKind::Keyboard => (0, d),
            },
            _ => (0, d),
        }
    }

    fn ev_bits(&self, ev_type: u16, mut d: [u8; 128]) -> (u8, [u8; 128]) {
        match (self.kind, ev_type) {
            (_, ev::SYN) => {
                set_bit(&mut d, ev::SYN_REPORT);
                (1, d)
            }
            (InputKind::Touchscreen { .. }, ev::ABS) => {
                for c in [
                    ev::ABS_X,
                    ev::ABS_Y,
                    ev::ABS_MT_SLOT,
                    ev::ABS_MT_POSITION_X,
                    ev::ABS_MT_POSITION_Y,
                    ev::ABS_MT_TRACKING_ID,
                ] {
                    set_bit(&mut d, c);
                }
                (8, d)
            }
            (InputKind::Touchscreen { .. }, ev::KEY) => {
                set_bit(&mut d, ev::BTN_TOUCH);
                (u8::try_from(ev::BTN_TOUCH / 8 + 1).unwrap_or(128), d)
            }
            (InputKind::Keyboard, ev::KEY) => {
                // KEY_ESC (1) .. KEY_MICMUTE (248).
                for c in 1..=248_u16 {
                    set_bit(&mut d, c);
                }
                (32, d)
            }
            _ => (0, d),
        }
    }
}

impl VirtioDevice for VirtioInput {
    fn device_type(&self) -> u32 {
        TYPE_INPUT
    }

    fn device_features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let (size, payload) = self.config_data();
        let mut cfg = [0_u8; 136];
        cfg[0] = self.select;
        cfg[1] = self.subsel;
        cfg[2] = size;
        cfg[8..].copy_from_slice(&payload);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            match offset + i as u64 {
                0 => self.select = b,
                1 => self.subsel = b,
                _ => {}
            }
        }
    }

    fn process_queue(
        &mut self,
        index: usize,
        queue: &mut Queue,
        mem: &dyn GuestMemory,
    ) -> Result<bool, QueueError> {
        let mut any = false;
        if index == 0 {
            // eventq: один буфер на событие; буферы берём, только если есть события.
            while !self.events.is_empty() && queue.pop(mem, &mut self.chain)? {
                let mut written = 0;
                if let (Some(d), Some(e)) = (self.chain.descriptors.first(), self.events.pop())
                    && d.is_write()
                    && d.len >= 8
                    && mem.write(d.addr, &e.bytes()).is_ok()
                {
                    written = 8;
                }
                queue.add_used(mem, self.chain.head, written)?;
                any = true;
            }
        } else {
            // statusq (светодиоды и т. п.) — подтверждаем и игнорируем.
            while queue.pop(mem, &mut self.chain)? {
                queue.add_used(mem, self.chain.head, 0)?;
                any = true;
            }
        }
        Ok(any)
    }
}

/// Помощник мультитача: превращает касания в события протокола B (слоты).
#[derive(Debug, Default)]
pub struct TouchState {
    next_tracking_id: i32,
    active: Vec<(u32, i32)>,
}

impl TouchState {
    fn tracking(&self, slot: u32) -> Option<i32> {
        self.active
            .iter()
            .find(|(s, _)| *s == slot)
            .map(|(_, id)| *id)
    }

    /// Касание или перемещение в слоте `slot`.
    pub fn touch(&mut self, slot: u32, x: u32, y: u32) -> Vec<InputEvent> {
        let abs = |code, value| InputEvent {
            kind: ev::ABS,
            code,
            value,
        };
        let mut out = vec![abs(ev::ABS_MT_SLOT, i32::try_from(slot).unwrap_or(0))];
        let first = self.active.is_empty();
        if self.tracking(slot).is_none() {
            self.next_tracking_id = (self.next_tracking_id + 1) & 0xFFFF;
            self.active.push((slot, self.next_tracking_id));
            out.push(abs(ev::ABS_MT_TRACKING_ID, self.next_tracking_id));
        }
        let (x, y) = (
            i32::try_from(x).unwrap_or(i32::MAX),
            i32::try_from(y).unwrap_or(i32::MAX),
        );
        out.extend([abs(ev::ABS_MT_POSITION_X, x), abs(ev::ABS_MT_POSITION_Y, y)]);
        if first {
            out.push(InputEvent {
                kind: ev::KEY,
                code: ev::BTN_TOUCH,
                value: 1,
            });
        }
        out.extend([abs(ev::ABS_X, x), abs(ev::ABS_Y, y)]);
        out.push(InputEvent {
            kind: ev::SYN,
            code: ev::SYN_REPORT,
            value: 0,
        });
        out
    }

    /// Отпускание слота `slot`.
    pub fn release(&mut self, slot: u32) -> Vec<InputEvent> {
        if self.tracking(slot).is_none() {
            return Vec::new();
        }
        self.active.retain(|(s, _)| *s != slot);
        let mut out = vec![
            InputEvent {
                kind: ev::ABS,
                code: ev::ABS_MT_SLOT,
                value: i32::try_from(slot).unwrap_or(0),
            },
            InputEvent {
                kind: ev::ABS,
                code: ev::ABS_MT_TRACKING_ID,
                value: -1,
            },
        ];
        if self.active.is_empty() {
            out.push(InputEvent {
                kind: ev::KEY,
                code: ev::BTN_TOUCH,
                value: 0,
            });
        }
        out.push(InputEvent {
            kind: ev::SYN,
            code: ev::SYN_REPORT,
            value: 0,
        });
        out
    }
}

/// События нажатия/отпускания клавиши (код Linux `KEY_*`).
#[must_use]
pub fn key_events(code: u16, pressed: bool) -> [InputEvent; 2] {
    [
        InputEvent {
            kind: ev::KEY,
            code,
            value: i32::from(pressed),
        },
        InputEvent {
            kind: ev::SYN,
            code: ev::SYN_REPORT,
            value: 0,
        },
    ]
}

#[cfg(test)]
mod tests {
    use rustdroid_virtio::mem::{GuestMemoryExt, VecMemory};
    use rustdroid_virtio::queue::VIRTQ_DESC_F_WRITE;

    use super::*;

    fn touch_dev() -> (VirtioInput, Arc<InputEvents>) {
        let events = Arc::new(InputEvents::default());
        let d = VirtioInput::new(
            InputKind::Touchscreen {
                width: 1080,
                height: 2400,
                slots: 10,
            },
            "RustDroid Touch",
            events.clone(),
        );
        (d, events)
    }

    fn cfg(d: &mut VirtioInput, select: u8, subsel: u8) -> (u8, Vec<u8>) {
        d.write_config(0, &[select, subsel]);
        let mut b = vec![0_u8; 136];
        d.read_config(0, &mut b);
        (b[2], b[8..].to_vec())
    }

    #[test]
    fn touchscreen_config_space() {
        let (mut d, _) = touch_dev();
        let (n, name) = cfg(&mut d, CFG_ID_NAME, 0);
        assert_eq!(&name[..usize::from(n)], b"RustDroid Touch");
        let (n, props) = cfg(&mut d, CFG_PROP_BITS, 0);
        assert_eq!((n, props[0]), (1, 1 << ev::INPUT_PROP_DIRECT));
        let (_, abs) = cfg(&mut d, CFG_EV_BITS, ev::ABS as u8);
        assert_ne!(
            abs[usize::from(ev::ABS_MT_POSITION_X / 8)] & (1 << (ev::ABS_MT_POSITION_X % 8)),
            0
        );
        let (n, info) = cfg(&mut d, CFG_ABS_INFO, ev::ABS_MT_POSITION_Y as u8);
        assert_eq!(n, 20);
        assert_eq!(u32::from_le_bytes(info[4..8].try_into().unwrap()), 2399);
        let (n, _) = cfg(&mut d, CFG_ABS_INFO, 0x20);
        assert_eq!(n, 0);
    }

    #[test]
    fn keyboard_reports_keys() {
        let mut d = VirtioInput::new(InputKind::Keyboard, "kbd", Arc::default());
        let (n, bits) = cfg(&mut d, CFG_EV_BITS, ev::KEY as u8);
        assert!(n > 0);
        assert_ne!(bits[0] & (1 << 1), 0); // KEY_ESC
        let (n, _) = cfg(&mut d, CFG_EV_BITS, ev::ABS as u8);
        assert_eq!(n, 0);
    }

    #[test]
    fn multitouch_protocol_b() {
        let mut t = TouchState::default();
        let down = t.touch(0, 100, 200);
        assert_eq!(
            down[1],
            InputEvent {
                kind: ev::ABS,
                code: ev::ABS_MT_TRACKING_ID,
                value: 1
            }
        );
        assert!(down.contains(&InputEvent {
            kind: ev::KEY,
            code: ev::BTN_TOUCH,
            value: 1
        }));
        assert_eq!(down.last().unwrap().kind, ev::SYN);
        // Второй палец: новый tracking id, BTN_TOUCH не повторяется.
        let second = t.touch(1, 5, 5);
        assert!(second.contains(&InputEvent {
            kind: ev::ABS,
            code: ev::ABS_MT_TRACKING_ID,
            value: 2
        }));
        assert!(!second.iter().any(|e| e.code == ev::BTN_TOUCH));
        // Движение первого пальца — без нового tracking id.
        assert!(
            !t.touch(0, 110, 210)
                .iter()
                .any(|e| e.code == ev::ABS_MT_TRACKING_ID)
        );
        let up0 = t.release(0);
        assert!(up0.contains(&InputEvent {
            kind: ev::ABS,
            code: ev::ABS_MT_TRACKING_ID,
            value: -1
        }));
        assert!(!up0.iter().any(|e| e.code == ev::BTN_TOUCH));
        let up1 = t.release(1);
        assert!(up1.contains(&InputEvent {
            kind: ev::KEY,
            code: ev::BTN_TOUCH,
            value: 0
        }));
        assert!(t.release(1).is_empty());
    }

    #[test]
    fn events_are_delivered_to_guest() {
        let (mut d, events) = touch_dev();
        let mem = VecMemory::new(0x10000);
        let mut q = Queue::new(8);
        (q.desc_table, q.avail_ring, q.used_ring, q.ready) = (0x1000, 0x2000, 0x3000, true);
        for i in 0..4_u16 {
            let mut b = [0_u8; 16];
            b[..8].copy_from_slice(&(0x5000 + u64::from(i) * 8).to_le_bytes());
            b[8..12].copy_from_slice(&8_u32.to_le_bytes());
            b[12..14].copy_from_slice(&VIRTQ_DESC_F_WRITE.to_le_bytes());
            mem.write(0x1000 + u64::from(i) * 16, &b).unwrap();
            mem.write_u16(0x2004 + u64::from(i) * 2, i).unwrap();
        }
        mem.write_u16(0x2002, 4).unwrap();
        assert!(!d.process_queue(0, &mut q, &mem).unwrap());
        events.push(&key_events(30, true)); // KEY_A
        assert!(d.process_queue(0, &mut q, &mem).unwrap());
        assert_eq!(mem.read_u16(0x3002).unwrap(), 2);
        let mut e = [0_u8; 8];
        mem.read(0x5000, &mut e).unwrap();
        assert_eq!(
            e,
            InputEvent {
                kind: ev::KEY,
                code: 30,
                value: 1
            }
            .bytes()
        );
    }
}
