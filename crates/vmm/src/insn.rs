//! Программная эмуляция инструкций, которых нет у CPU хоста.
//!
//! Образы Android x86_64 собираются под Silvermont и используют `MOVBE` (загрузка/запись с
//! перестановкой байтов). У Intel до Haswell (Sandy/Ivy Bridge, 2011–2012) и у AMD до
//! Jaguar/Zen её нет: гость получает `#UD` и падает уже в `init`. VMM перехватывает `#UD`,
//! декодирует `MOVBE` и выполняет её сам; остальные `#UD` возвращает гостю как есть.
//!
//! Так же выполняются инструкции AES-NI (см. [`crate::aesni`]): у бюджетных Core i3 того же
//! поколения их нет, а BoringSSL в Android их требует.
//!
//! Здесь только декодирование и арифметика (тестируются на любой платформе); доступ к
//! регистрам и памяти гостя — в цикле vCPU.

use crate::aesni::AesOp;

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

/// Операнд ModRM.r/m.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operand {
    /// Регистр (0–15): общего назначения или XMM, по контексту инструкции.
    Reg(u8),
    /// Память.
    Mem(MemOperand),
}

/// Декодированная инструкция AES-NI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aes {
    /// Длина инструкции, байт.
    pub len: u8,
    /// Операция.
    pub op: AesOp,
    /// Регистр XMM назначения (первый операнд).
    pub dst: u8,
    /// Второй операнд: XMM или 16 байт памяти.
    pub src: Operand,
}

/// Инструкция, которую VMM умеет выполнить программно.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insn {
    /// `MOVBE`.
    Movbe(Movbe),
    /// AES-NI.
    Aes(Aes),
}

#[derive(Debug, Default)]
struct Prefixes {
    len: usize,
    opsize16: bool,
    addr32: bool,
    segment: Option<Segment>,
    rex: u8,
    rep: bool,
}

fn parse_prefixes(bytes: &[u8]) -> Option<Prefixes> {
    let mut p = Prefixes::default();
    loop {
        let b = *bytes.get(p.len)?;
        match b {
            0x66 => p.opsize16 = true,
            0x67 => p.addr32 = true,
            0x64 => p.segment = Some(Segment::Fs),
            0x65 => p.segment = Some(Segment::Gs),
            0x26 | 0x2E | 0x36 | 0x3E => {}
            // F2/F3 меняют смысл опкода (например, F2 0F 38 F1 — CRC32), LOCK — #UD.
            0xF2 | 0xF3 => p.rep = true,
            0xF0 => return None,
            0x40..=0x4F => {
                // REX — последний префикс перед опкодом; два REX подряд не поддерживаем.
                p.rex = b;
                p.len += 1;
                if bytes.get(p.len).is_some_and(|b| (0x40..=0x4F).contains(b)) {
                    return None;
                }
                return Some(p);
            }
            _ => return Some(p),
        }
        p.len += 1;
        if p.len > 14 {
            return None;
        }
    }
}

/// Разбирает ModRM (+SIB, смещение) с позиции `i`. Возвращает (новую позицию, поле reg с
/// REX.R, операнд r/m).
fn parse_modrm(bytes: &[u8], mut i: usize, p: &Prefixes) -> Option<(usize, u8, Operand)> {
    let modrm = *bytes.get(i)?;
    i += 1;
    let md = modrm >> 6;
    let rex_r = (p.rex >> 2) & 1;
    let rex_x = (p.rex >> 1) & 1;
    let rex_b = p.rex & 1;
    let reg = ((modrm >> 3) & 7) | (rex_r << 3);
    let rm = modrm & 7;
    if md == 3 {
        return Some((i, reg, Operand::Reg(rm | (rex_b << 3))));
    }
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
    Some((
        i,
        reg,
        Operand::Mem(MemOperand {
            base,
            index,
            disp,
            rip_relative,
            segment: p.segment,
            addr32: p.addr32,
        }),
    ))
}

/// Декодирует инструкцию, которую VMM умеет выполнить программно. `None` — другая
/// инструкция (или форма, которой нет и на реальном CPU): такой `#UD` возвращается гостю.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<Insn> {
    let p = parse_prefixes(bytes)?;
    let i = p.len;
    let op = bytes.get(i..i + 3)?;
    match op {
        // MOVBE: 0F 38 F0 (загрузка) / F1 (запись), только с памятью и без F2/F3.
        [0x0F, 0x38, 0xF0 | 0xF1] if !p.rep => {
            let (end, reg, Operand::Mem(mem)) = parse_modrm(bytes, i + 3, &p)? else {
                return None;
            };
            Some(Insn::Movbe(Movbe {
                len: u8::try_from(end).ok()?,
                direction: if op[2] == 0xF0 {
                    Direction::Load
                } else {
                    Direction::Store
                },
                size: if p.rex & 8 != 0 {
                    8
                } else if p.opsize16 {
                    2
                } else {
                    4
                },
                reg,
                mem,
            }))
        }
        // AES-NI: обязательный префикс 66, без F2/F3.
        [0x0F, 0x38, 0xDB..=0xDF] | [0x0F, 0x3A, 0xDF | 0x44] if p.opsize16 && !p.rep => {
            let (mut end, dst, src) = parse_modrm(bytes, i + 3, &p)?;
            let aes = match op {
                [0x0F, 0x38, 0xDB] => AesOp::Imc,
                [0x0F, 0x38, 0xDC] => AesOp::Enc,
                [0x0F, 0x38, 0xDD] => AesOp::EncLast,
                [0x0F, 0x38, 0xDE] => AesOp::Dec,
                [0x0F, 0x38, 0xDF] => AesOp::DecLast,
                [0x0F, 0x3A, 0x44] => {
                    let imm = *bytes.get(end)?;
                    end += 1;
                    AesOp::Pclmul(imm)
                }
                _ => {
                    let imm = *bytes.get(end)?;
                    end += 1;
                    AesOp::KeygenAssist(imm)
                }
            };
            Some(Insn::Aes(Aes {
                len: u8::try_from(end).ok()?,
                op: aes,
                dst,
                src,
            }))
        }
        _ => None,
    }
}

/// Декодирует `MOVBE` (см. [`decode`]).
#[must_use]
pub fn decode_movbe(bytes: &[u8]) -> Option<Movbe> {
    match decode(bytes)? {
        Insn::Movbe(m) => Some(m),
        Insn::Aes(_) => None,
    }
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
    fn decodes_aesni() {
        // aesenclast %xmm4,%xmm0 — инструкция, на которой падал libcrypto на i3-3220.
        let Some(Insn::Aes(a)) = decode(&[0x66, 0x0F, 0x38, 0xDD, 0xC4, 0x66]) else {
            panic!("не AES");
        };
        assert_eq!(
            (a.len, a.op, a.dst, a.src),
            (5, AesOp::EncLast, 0, Operand::Reg(4))
        );
        // aesenc 0x10(%rdi),%xmm9 : 66 44 0F 38 DC 4F 10
        let Some(Insn::Aes(a)) = decode(&[0x66, 0x44, 0x0F, 0x38, 0xDC, 0x4F, 0x10]) else {
            panic!("не AES");
        };
        assert_eq!((a.len, a.op, a.dst), (7, AesOp::Enc, 9));
        assert!(matches!(a.src, Operand::Mem(m) if m.base == Some(7) && m.disp == 0x10));
        // aeskeygenassist $0x1b,%xmm1,%xmm2 : 66 0F 3A DF D1 1B
        let Some(Insn::Aes(a)) = decode(&[0x66, 0x0F, 0x3A, 0xDF, 0xD1, 0x1B]) else {
            panic!("не AES");
        };
        assert_eq!(
            (a.len, a.op, a.dst, a.src),
            (6, AesOp::KeygenAssist(0x1B), 2, Operand::Reg(1))
        );
        // aesimc %xmm3,%xmm3 и aesdec/aesdeclast.
        assert!(matches!(
            decode(&[0x66, 0x0F, 0x38, 0xDB, 0xDB]),
            Some(Insn::Aes(Aes { op: AesOp::Imc, .. }))
        ));
        assert!(matches!(
            decode(&[0x66, 0x0F, 0x38, 0xDE, 0xC1]),
            Some(Insn::Aes(Aes { op: AesOp::Dec, .. }))
        ));
        assert!(matches!(
            decode(&[0x66, 0x0F, 0x38, 0xDF, 0xC1]),
            Some(Insn::Aes(Aes {
                op: AesOp::DecLast,
                ..
            }))
        ));
        // pclmulqdq $0x11,%xmm1,%xmm0 : 66 0F 3A 44 C1 11
        assert!(matches!(
            decode(&[0x66, 0x0F, 0x3A, 0x44, 0xC1, 0x11]),
            Some(Insn::Aes(Aes {
                len: 6,
                op: AesOp::Pclmul(0x11),
                dst: 0,
                src: Operand::Reg(1)
            }))
        ));
        // Без префикса 66 это не AES-NI.
        assert!(decode(&[0x0F, 0x38, 0xDD, 0xC4]).is_none());
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
