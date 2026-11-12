//! Программная реализация инструкций AES-NI (`AESENC`, `AESENCLAST`, `AESDEC`, `AESDECLAST`,
//! `AESIMC`, `AESKEYGENASSIST`) по описанию Intel SDM.
//!
//! Нужна на CPU без AES-NI (например, Core i3 Sandy/Ivy Bridge): BoringSSL в образах Android
//! собран с `-maes` и не проверяет CPUID. Регистр XMM — 16 байт, байт `i` — элемент состояния
//! AES в строке `i % 4`, столбце `i / 4`.

/// Операция AES-NI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AesOp {
    /// `AESENC`: раунд шифрования.
    Enc,
    /// `AESENCLAST`: последний раунд шифрования.
    EncLast,
    /// `AESDEC`: раунд расшифровки.
    Dec,
    /// `AESDECLAST`: последний раунд расшифровки.
    DecLast,
    /// `AESIMC`: InvMixColumns ключа раунда.
    Imc,
    /// `AESKEYGENASSIST imm8`: помощь в расширении ключа.
    KeygenAssist(u8),
    /// `PCLMULQDQ imm8`: умножение без переносов (GHASH в AES-GCM). Бит 0 imm выбирает
    /// половину первого операнда, бит 4 — второго.
    Pclmul(u8),
}

const SBOX: [u8; 256] = [
    0x63, 0x7C, 0x77, 0x7B, 0xF2, 0x6B, 0x6F, 0xC5, 0x30, 0x01, 0x67, 0x2B, 0xFE, 0xD7, 0xAB, 0x76,
    0xCA, 0x82, 0xC9, 0x7D, 0xFA, 0x59, 0x47, 0xF0, 0xAD, 0xD4, 0xA2, 0xAF, 0x9C, 0xA4, 0x72, 0xC0,
    0xB7, 0xFD, 0x93, 0x26, 0x36, 0x3F, 0xF7, 0xCC, 0x34, 0xA5, 0xE5, 0xF1, 0x71, 0xD8, 0x31, 0x15,
    0x04, 0xC7, 0x23, 0xC3, 0x18, 0x96, 0x05, 0x9A, 0x07, 0x12, 0x80, 0xE2, 0xEB, 0x27, 0xB2, 0x75,
    0x09, 0x83, 0x2C, 0x1A, 0x1B, 0x6E, 0x5A, 0xA0, 0x52, 0x3B, 0xD6, 0xB3, 0x29, 0xE3, 0x2F, 0x84,
    0x53, 0xD1, 0x00, 0xED, 0x20, 0xFC, 0xB1, 0x5B, 0x6A, 0xCB, 0xBE, 0x39, 0x4A, 0x4C, 0x58, 0xCF,
    0xD0, 0xEF, 0xAA, 0xFB, 0x43, 0x4D, 0x33, 0x85, 0x45, 0xF9, 0x02, 0x7F, 0x50, 0x3C, 0x9F, 0xA8,
    0x51, 0xA3, 0x40, 0x8F, 0x92, 0x9D, 0x38, 0xF5, 0xBC, 0xB6, 0xDA, 0x21, 0x10, 0xFF, 0xF3, 0xD2,
    0xCD, 0x0C, 0x13, 0xEC, 0x5F, 0x97, 0x44, 0x17, 0xC4, 0xA7, 0x7E, 0x3D, 0x64, 0x5D, 0x19, 0x73,
    0x60, 0x81, 0x4F, 0xDC, 0x22, 0x2A, 0x90, 0x88, 0x46, 0xEE, 0xB8, 0x14, 0xDE, 0x5E, 0x0B, 0xDB,
    0xE0, 0x32, 0x3A, 0x0A, 0x49, 0x06, 0x24, 0x5C, 0xC2, 0xD3, 0xAC, 0x62, 0x91, 0x95, 0xE4, 0x79,
    0xE7, 0xC8, 0x37, 0x6D, 0x8D, 0xD5, 0x4E, 0xA9, 0x6C, 0x56, 0xF4, 0xEA, 0x65, 0x7A, 0xAE, 0x08,
    0xBA, 0x78, 0x25, 0x2E, 0x1C, 0xA6, 0xB4, 0xC6, 0xE8, 0xDD, 0x74, 0x1F, 0x4B, 0xBD, 0x8B, 0x8A,
    0x70, 0x3E, 0xB5, 0x66, 0x48, 0x03, 0xF6, 0x0E, 0x61, 0x35, 0x57, 0xB9, 0x86, 0xC1, 0x1D, 0x9E,
    0xE1, 0xF8, 0x98, 0x11, 0x69, 0xD9, 0x8E, 0x94, 0x9B, 0x1E, 0x87, 0xE9, 0xCE, 0x55, 0x28, 0xDF,
    0x8C, 0xA1, 0x89, 0x0D, 0xBF, 0xE6, 0x42, 0x68, 0x41, 0x99, 0x2D, 0x0F, 0xB0, 0x54, 0xBB, 0x16,
];
const INV_SBOX: [u8; 256] = [
    0x52, 0x09, 0x6A, 0xD5, 0x30, 0x36, 0xA5, 0x38, 0xBF, 0x40, 0xA3, 0x9E, 0x81, 0xF3, 0xD7, 0xFB,
    0x7C, 0xE3, 0x39, 0x82, 0x9B, 0x2F, 0xFF, 0x87, 0x34, 0x8E, 0x43, 0x44, 0xC4, 0xDE, 0xE9, 0xCB,
    0x54, 0x7B, 0x94, 0x32, 0xA6, 0xC2, 0x23, 0x3D, 0xEE, 0x4C, 0x95, 0x0B, 0x42, 0xFA, 0xC3, 0x4E,
    0x08, 0x2E, 0xA1, 0x66, 0x28, 0xD9, 0x24, 0xB2, 0x76, 0x5B, 0xA2, 0x49, 0x6D, 0x8B, 0xD1, 0x25,
    0x72, 0xF8, 0xF6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xD4, 0xA4, 0x5C, 0xCC, 0x5D, 0x65, 0xB6, 0x92,
    0x6C, 0x70, 0x48, 0x50, 0xFD, 0xED, 0xB9, 0xDA, 0x5E, 0x15, 0x46, 0x57, 0xA7, 0x8D, 0x9D, 0x84,
    0x90, 0xD8, 0xAB, 0x00, 0x8C, 0xBC, 0xD3, 0x0A, 0xF7, 0xE4, 0x58, 0x05, 0xB8, 0xB3, 0x45, 0x06,
    0xD0, 0x2C, 0x1E, 0x8F, 0xCA, 0x3F, 0x0F, 0x02, 0xC1, 0xAF, 0xBD, 0x03, 0x01, 0x13, 0x8A, 0x6B,
    0x3A, 0x91, 0x11, 0x41, 0x4F, 0x67, 0xDC, 0xEA, 0x97, 0xF2, 0xCF, 0xCE, 0xF0, 0xB4, 0xE6, 0x73,
    0x96, 0xAC, 0x74, 0x22, 0xE7, 0xAD, 0x35, 0x85, 0xE2, 0xF9, 0x37, 0xE8, 0x1C, 0x75, 0xDF, 0x6E,
    0x47, 0xF1, 0x1A, 0x71, 0x1D, 0x29, 0xC5, 0x89, 0x6F, 0xB7, 0x62, 0x0E, 0xAA, 0x18, 0xBE, 0x1B,
    0xFC, 0x56, 0x3E, 0x4B, 0xC6, 0xD2, 0x79, 0x20, 0x9A, 0xDB, 0xC0, 0xFE, 0x78, 0xCD, 0x5A, 0xF4,
    0x1F, 0xDD, 0xA8, 0x33, 0x88, 0x07, 0xC7, 0x31, 0xB1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xEC, 0x5F,
    0x60, 0x51, 0x7F, 0xA9, 0x19, 0xB5, 0x4A, 0x0D, 0x2D, 0xE5, 0x7A, 0x9F, 0x93, 0xC9, 0x9C, 0xEF,
    0xA0, 0xE0, 0x3B, 0x4D, 0xAE, 0x2A, 0xF5, 0xB0, 0xC8, 0xEB, 0xBB, 0x3C, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2B, 0x04, 0x7E, 0xBA, 0x77, 0xD6, 0x26, 0xE1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0C, 0x7D,
];

fn xtime(b: u8) -> u8 {
    (b << 1) ^ if b & 0x80 == 0 { 0 } else { 0x1B }
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

fn shift_rows(s: &[u8; 16]) -> [u8; 16] {
    let mut o = [0; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[r + 4 * c] = s[r + 4 * ((c + r) % 4)];
        }
    }
    o
}

fn inv_shift_rows(s: &[u8; 16]) -> [u8; 16] {
    let mut o = [0; 16];
    for c in 0..4 {
        for r in 0..4 {
            o[r + 4 * c] = s[r + 4 * ((c + 4 - r) % 4)];
        }
    }
    o
}

fn sub_bytes(s: &mut [u8; 16], table: &[u8; 256]) {
    for b in s.iter_mut() {
        *b = table[usize::from(*b)];
    }
}

fn mix_columns(s: &mut [u8; 16], m: [u8; 4]) {
    for c in 0..4 {
        let col = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
        for r in 0..4 {
            s[4 * c + r] = (0..4).fold(0, |acc, k| acc ^ gmul(m[(k + 4 - r) % 4], col[k]));
        }
    }
}

const MIX: [u8; 4] = [2, 3, 1, 1];
const INV_MIX: [u8; 4] = [14, 11, 13, 9];

fn sub_word(w: u32) -> u32 {
    u32::from_le_bytes(w.to_le_bytes().map(|b| SBOX[usize::from(b)]))
}

fn xor(a: [u8; 16], b: &[u8; 16]) -> [u8; 16] {
    let mut o = a;
    for (x, y) in o.iter_mut().zip(b) {
        *x ^= y;
    }
    o
}

/// Выполняет `op`: `dst` — первый операнд (состояние), `src` — второй (ключ раунда или
/// исходные данные для `AESIMC`/`AESKEYGENASSIST`). Возвращает новое значение `dst`.
#[must_use]
pub fn execute(op: AesOp, dst: &[u8; 16], src: &[u8; 16]) -> [u8; 16] {
    match op {
        AesOp::Enc => {
            let mut s = shift_rows(dst);
            sub_bytes(&mut s, &SBOX);
            mix_columns(&mut s, MIX);
            xor(s, src)
        }
        AesOp::EncLast => {
            let mut s = shift_rows(dst);
            sub_bytes(&mut s, &SBOX);
            xor(s, src)
        }
        AesOp::Dec => {
            let mut s = inv_shift_rows(dst);
            sub_bytes(&mut s, &INV_SBOX);
            mix_columns(&mut s, INV_MIX);
            xor(s, src)
        }
        AesOp::DecLast => {
            let mut s = inv_shift_rows(dst);
            sub_bytes(&mut s, &INV_SBOX);
            xor(s, src)
        }
        AesOp::Imc => {
            let mut s = *src;
            mix_columns(&mut s, INV_MIX);
            s
        }
        AesOp::Pclmul(imm) => {
            let half = |v: &[u8; 16], hi: bool| {
                let mut b = [0_u8; 8];
                b.copy_from_slice(if hi { &v[8..] } else { &v[..8] });
                u64::from_le_bytes(b)
            };
            let (a, b) = (half(dst, imm & 1 != 0), half(src, imm & 0x10 != 0));
            let mut r: u128 = 0;
            for i in 0..64 {
                if b >> i & 1 != 0 {
                    r ^= u128::from(a) << i;
                }
            }
            r.to_le_bytes()
        }
        AesOp::KeygenAssist(rcon) => {
            let word = |i: usize| {
                u32::from_le_bytes([src[4 * i], src[4 * i + 1], src[4 * i + 2], src[4 * i + 3]])
            };
            let (x1, x3) = (sub_word(word(1)), sub_word(word(3)));
            let rcon = u32::from(rcon);
            let words = [x1, x1.rotate_right(8) ^ rcon, x3, x3.rotate_right(8) ^ rcon];
            let mut o = [0; 16];
            for (i, w) in words.iter().enumerate() {
                o[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
            }
            o
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> [u8; 16] {
        let mut o = [0; 16];
        for (i, b) in o.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        o
    }

    /// Расширение ключа AES-128 так, как его делает BoringSSL на AES-NI: AESKEYGENASSIST
    /// и сдвиги.
    fn expand(key: [u8; 16]) -> Vec<[u8; 16]> {
        let mut keys = vec![key];
        for rcon in [1, 2, 4, 8, 16, 32, 64, 128, 0x1B, 0x36] {
            let prev = *keys.last().unwrap();
            let t = execute(AesOp::KeygenAssist(rcon), &[0; 16], &prev);
            let t3 = [t[12], t[13], t[14], t[15]];
            let mut k = [0_u8; 16];
            let mut acc = [0_u8; 4];
            for w in 0..4 {
                for b in 0..4 {
                    acc[b] ^= prev[4 * w + b];
                    k[4 * w + b] = acc[b] ^ t3[b];
                }
            }
            keys.push(k);
        }
        keys
    }

    #[test]
    fn pclmulqdq_selects_halves_and_multiplies() {
        let a = 0x7b5b_5465_7374_5665_u64;
        let b = 0x4869_2853_6861_7929_u64;
        let c = 0x5b47_7565_726f_6e5d_u64;
        let mut x = [0_u8; 16];
        x[..8].copy_from_slice(&a.to_le_bytes());
        x[8..].copy_from_slice(&1_u64.to_le_bytes());
        let mut y = [0_u8; 16];
        y[..8].copy_from_slice(&c.to_le_bytes());
        y[8..].copy_from_slice(&b.to_le_bytes());
        let mul = |imm| u128::from_le_bytes(execute(AesOp::Pclmul(imm), &x, &y));
        // a·c, посчитано независимо (Python, умножение многочленов над GF(2)).
        assert_eq!(mul(0x00), 0x1a2b_f6db_3a30_862f_babf_262d_f4b7_d5c9);
        // Старшая половина x равна 1: 1·c и 1·b.
        assert_eq!(mul(0x01), u128::from(c));
        assert_eq!(mul(0x11), u128::from(b));
        // Дистрибутивность: a·(b ⊕ c) = a·b ⊕ a·c.
        let mut z = y;
        z[..8].copy_from_slice(&(b ^ c).to_le_bytes());
        let abc = u128::from_le_bytes(execute(AesOp::Pclmul(0x00), &x, &z));
        assert_eq!(abc, mul(0x00) ^ mul(0x10));
    }

    #[test]
    fn fips197_aes128_encrypt_and_decrypt() {
        // FIPS-197, приложение C.1.
        let key = hex("000102030405060708090a0b0c0d0e0f");
        let pt = hex("00112233445566778899aabbccddeeff");
        let ct = hex("69c4e0d86a7b0430d8cdb78070b4c55a");
        let keys = expand(key);
        assert_eq!(keys[10], hex("13111d7fe3944a17f307a78b4d2b30c5"));

        let mut s = xor(pt, &keys[0]);
        for k in &keys[1..10] {
            s = execute(AesOp::Enc, &s, k);
        }
        s = execute(AesOp::EncLast, &s, &keys[10]);
        assert_eq!(s, ct);

        // Расшифровка: ключи раундов 1–9 через AESIMC (эквивалентная обратная схема).
        let mut s = xor(ct, &keys[10]);
        for k in keys[1..10].iter().rev() {
            let dk = execute(AesOp::Imc, &[0; 16], k);
            s = execute(AesOp::Dec, &s, &dk);
        }
        s = execute(AesOp::DecLast, &s, &keys[0]);
        assert_eq!(s, pt);
    }
}
