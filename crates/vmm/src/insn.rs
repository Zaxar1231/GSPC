//! Программная эмуляция инструкций, которых нет у CPU хоста.
//!
//! Образы Android x86_64 собираются под Silvermont и используют `MOVBE` (загрузка/запись с
//! перестановкой байтов). У Intel до Haswell (Sandy/Ivy Bridge, 2011–2012) и у AMD до
//! Jaguar/Zen её нет: гость получает `#UD` и падает уже в `init`. VMM перехватывает `#UD`,
//! декодирует `MOVBE` и выполняет её сам; остальные `#UD` возвращает гостю как есть.
//!
//! Здесь только декодирование и арифметика (тестируются на любой платформе); доступ к
//! регистрам и памяти гостя — в цикле vCPU.

/// Сегмент с ненулевой базой в 64-битном режиме.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// `fs:` (TLS в пользовательском режиме Linux).
    Fs,
    /// `gs:` (per-CPU данные ядра).
    Gs,
}

/// Операнд в памяти: `seg:[base + index*scale + disp]` или `[rip + disp]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemOperand {
    /// Базовый регистр (0–15, порядок кодирования x86).
    pub base: Option<u8>,
    /// Индексный регистр и масштаб (1, 2, 4, 8).
    pub index: Option<(u8, u8)>,
    /// Смещение.
    pub disp: i64,
    /// Адресация относительно RIP следующей инструкции.
    pub rip_relative: bool,
    /// Префикс сегмента.
    pub segment: Option<Segment>,
    /// Префикс `0x67`: 32-битная адресация.
    pub addr32: bool,
}

/// Направление `MOVBE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `0F 38 F0`: регистр ← память.
    Load,
    /// `0F 38 F1`: память ← регистр.
    Store,
}

/// Декодированная `MOVBE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Movbe {
    /// Длина инструкции, байт.
    pub len: u8,
    /// Направление.
    pub direction: Direction,
    /// Размер операнда: 2, 4 или 8 байт.
    pub size: u8,
    /// Регистр (0–15).
    pub reg: u8,
    /// Операнд в памяти.
    pub mem: MemOperand,
}

impl MemOperand {
    /// Эффективный (линейный) адрес. `gpr` — 16 регистров общего назначения, `next_rip` —
    /// адрес следующей инструкции, `fs_base`/`gs_base` — базы сегментов.
    #[must_use]
    pub fn address(&self, gpr: &[u64; 16], next_rip: u64, fs_base: u64, gs_base: u64) -> u64 {
        let mut ea = if self.rip_relative { next_rip } else { 0 };
        if let Some(b) = self.base {
            ea = ea.wrapping_add(gpr[usize::from(b & 15)]);
        }
        if let Some((i, scale)) = self.index {
            ea = ea.wrapping_add(gpr[usize::from(i & 15)].wrapping_mul(u64::from(scale)));
        }
        #[allow(
            clippy::cast_sign_loss,
            reason = "смещение складывается по модулю 2^64"
        )]
        let disp = self.disp as u64;
        ea = ea.wrapping_add(disp);
        if self.addr32 {
            ea &= 0xFFFF_FFFF;
        }
        ea.wrapping_add(match self.segment {
            Some(Segment::Fs) => fs_base,
            Some(Segment::Gs) => gs_base,
            None => 0,
        })
    }
}

/// Декодирует `MOVBE` в 64-битном режиме. `None` — это не `MOVBE` (или форма, которой на
/// реальном CPU тоже нет, например регистр–регистр): такой `#UD` возвращается гостю.
#[must_use]
pub fn decode_movbe(bytes: &[u8]) -> Option<Movbe> {
    let mut i = 0_usize;
    let mut opsize16 = false;
    let mut addr32 = false;
    let mut segment = None;
    let mut rex = 0_u8;
    // Префиксы (не больше 14 байт до опкода, как у CPU).
    loop {
        let b = *bytes.get(i)?;
        match b {
            0x66 => opsize16 = true,
            0x67 => addr32 = true,
            0x64 => segment = Some(Segment::Fs),
            0x65 => segment = Some(Segment::Gs),
            0x26 | 0x2E | 0x36 | 0x3E => {}
            // F2 0F 38 F0/F1 — CRC32 (есть у хоста), F3 — недопустимо, LOCK — #UD.
            0xF0 | 0xF2 | 0xF3 => return None,
            0x40..=0x4F => {
                // REX — последний префикс перед опкодом.
                rex = b;
                i += 1;
                break;
            }
            _ => break,
        }
        i += 1;
        if i > 14 {
            return None;
        }
    }
    if rex != 0 && bytes.get(i).is_some_and(|b| (0x40..=0x4F).contains(b)) {
        return None;
    }
    if bytes.get(i..i + 3)? != [0x0F, 0x38, 0xF0] && bytes.get(i..i + 3)? != [0x0F, 0x38, 0xF1] {
        return None;
    }
    let direction = if bytes[i + 2] == 0xF0 {
        Direction::Load
    } else {
        Direction::Store
    };
    i += 3;
    let modrm = *bytes.get(i)?;
    i += 1;
    let md = modrm >> 6;
    if md == 3 {
        return None;
    }
    let rex_w = rex & 8 != 0;
    let rex_r = (rex >> 2) & 1;
    let rex_x = (rex >> 1) & 1;
    let rex_b = rex & 1;
    let reg = ((modrm >> 3) & 7) | (rex_r << 3);
    let rm = modrm & 7;

    let mut base = None;
    let mut index = None;
    let mut rip_relative = false;
    let mut disp_len = match md {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    if rm == 4 {
        let sib = *bytes.get(i)?;
        i += 1;
        let scale = 1_u8 << (sib >> 6);
        let idx = ((sib >> 3) & 7) | (rex_x << 3);
        if idx != 4 {
            index = Some((idx, scale));
        }
        let b = sib & 7;
        if b == 5 && md == 0 {
            disp_len = 4;
        } else {
            base = Some(b | (rex_b << 3));
        }
    } else if rm == 5 && md == 0 {
        rip_relative = true;
        disp_len = 4;
    } else {
        base = Some(rm | (rex_b << 3));
    }
    let disp = match disp_len {
        1 => i64::from(i8::from_le_bytes([*bytes.get(i)?])),
        4 => i64::from(i32::from_le_bytes(bytes.get(i..i + 4)?.try_into().ok()?)),
        _ => 0,
    };
    i += disp_len;
    let size = if rex_w {
        8
    } else if opsize16 {
        2
    } else {
        4
    };
    Some(Movbe {
        len: u8::try_from(i).ok()?,
        direction,
        size,
        reg,
        mem: MemOperand {
            base,
            index,
            disp,
            rip_relative,
            segment,
            addr32,
        },
    })
}

/// Значение регистра после `MOVBE` reg ← mem: 32-битная запись обнуляет старшую половину,
/// 16-битная сохраняет старшие 48 бит.
#[must_use]
pub fn load_result(old: u64, mem: &[u8], size: u8) -> u64 {
    match size {
        2 => (old & !0xFFFF) | u64::from(u16::from_be_bytes([mem[0], mem[1]])),
        4 => u64::from(u32::from_be_bytes([mem[0], mem[1], mem[2], mem[3]])),
        _ => u64::from_be_bytes([
            mem[0], mem[1], mem[2], mem[3], mem[4], mem[5], mem[6], mem[7],
        ]),
    }
}

/// Байты для записи в память при `MOVBE` mem ← reg.
#[must_use]
pub fn store_bytes(value: u64, size: u8) -> Vec<u8> {
    let be = value.to_be_bytes();
    be[8 - usize::from(size)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_store_from_android_init() {
        // movbe %ecx,0x60(%rbx) — инструкция, на которой падал init на i3-3220.
        let m = decode_movbe(&[0x0F, 0x38, 0xF1, 0x4B, 0x60, 0x0F]).unwrap();
        assert_eq!(m.len, 5);
        assert_eq!(m.direction, Direction::Store);
        assert_eq!((m.size, m.reg), (4, 1));
        assert_eq!(m.mem.base, Some(3));
        assert_eq!(m.mem.disp, 0x60);
    }

    #[test]
    fn decodes_rex_sib_and_prefixes() {
        // movbe 0x10(%r12,%r13,8),%r9  : 4F 0F 38 F0 4C EC 10
        let m = decode_movbe(&[0x4F, 0x0F, 0x38, 0xF0, 0x4C, 0xEC, 0x10]).unwrap();
        assert_eq!((m.len, m.size, m.reg), (7, 8, 9));
        assert_eq!(m.direction, Direction::Load);
        assert_eq!(m.mem.base, Some(12));
        assert_eq!(m.mem.index, Some((13, 8)));
        // movbe %fs:0x28,%ax : 64 66 0F 38 F0 04 25 28 00 00 00
        let m = decode_movbe(&[0x64, 0x66, 0x0F, 0x38, 0xF0, 0x04, 0x25, 0x28, 0, 0, 0]).unwrap();
        assert_eq!((m.len, m.size, m.reg), (11, 2, 0));
        assert_eq!((m.mem.base, m.mem.index), (None, None));
        assert_eq!(m.mem.segment, Some(Segment::Fs));
        assert_eq!(m.mem.disp, 0x28);
        // movbe -0x8(%rip),%edx : 0F 38 F0 15 F8 FF FF FF
        let m = decode_movbe(&[0x0F, 0x38, 0xF0, 0x15, 0xF8, 0xFF, 0xFF, 0xFF]).unwrap();
        assert!(m.mem.rip_relative);
        assert_eq!(m.mem.disp, -8);
        assert_eq!(m.mem.address(&[0; 16], 0x1000, 0, 0), 0xFF8);
    }

    #[test]
    fn rejects_non_movbe() {
        // CRC32 (F2 0F 38 F1), регистровая форма, ud2, обрезанная инструкция.
        assert!(decode_movbe(&[0xF2, 0x0F, 0x38, 0xF1, 0xC1]).is_none());
        assert!(decode_movbe(&[0x0F, 0x38, 0xF0, 0xC1]).is_none());
        assert!(decode_movbe(&[0x0F, 0x0B]).is_none());
        assert!(decode_movbe(&[0x0F, 0x38, 0xF0, 0x44]).is_none());
        // Два REX подряд компиляторы не генерируют — считаем это не MOVBE.
        assert!(decode_movbe(&[0x48, 0x48, 0x0F, 0x38, 0xF0, 0x00]).is_none());
    }

    #[test]
    fn effective_address_and_data() {
        let mut gpr = [0_u64; 16];
        gpr[3] = 0x7000;
        gpr[12] = 0x100;
        gpr[13] = 2;
        let m = decode_movbe(&[0x0F, 0x38, 0xF1, 0x4B, 0x60]).unwrap();
        assert_eq!(m.mem.address(&gpr, 0, 0, 0), 0x7060);
        let m = decode_movbe(&[0x4F, 0x0F, 0x38, 0xF0, 0x4C, 0xEC, 0x10]).unwrap();
        assert_eq!(m.mem.address(&gpr, 0, 0, 0), 0x100 + 16 + 0x10);

        assert_eq!(
            load_result(u64::MAX, &[0x12, 0x34, 0x56, 0x78], 4),
            0x1234_5678
        );
        assert_eq!(
            load_result(0xAAAA_BBBB_CCCC_DDDD, &[0x12, 0x34], 2),
            0xAAAA_BBBB_CCCC_1234
        );
        assert_eq!(store_bytes(0x1122_3344, 4), [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(
            store_bytes(0x1122_3344_5566_7788, 8),
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
    }
}
