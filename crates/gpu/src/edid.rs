//! Генерация EDID 1.4 с режимом по CVT reduced blanking.
//!
//! Через EDID гость узнаёт разрешение и частоту дисплея (60/90/120/144 Гц). Ядро (DRM
//! virtio-gpu) строит из него режимы, Android берёт частоту отсюда же, поэтому игры видят
//! высокую частоту дисплея.

/// Режим дисплея.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    /// Ширина, пиксели.
    pub width: u32,
    /// Высота, пиксели.
    pub height: u32,
    /// Частота, Гц.
    pub refresh_hz: u32,
}

/// Тайминги CVT-RB (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Пиксельная частота, кГц.
    pub pixel_clock_khz: u32,
    /// Горизонтальный бланк.
    pub h_blank: u32,
    /// Front porch по горизонтали.
    pub h_front: u32,
    /// Ширина импульса синхронизации по горизонтали.
    pub h_sync: u32,
    /// Вертикальный бланк, строки.
    pub v_blank: u32,
    /// Front porch по вертикали.
    pub v_front: u32,
    /// Синхроимпульс по вертикали.
    pub v_sync: u32,
}

/// Наибольшая пиксельная частота, которую выражает 18-байтный DTD (655,35 МГц).
pub const MAX_DTD_PIXEL_CLOCK_KHZ: u32 = 655_350;

impl DisplayMode {
    /// Тайминги CVT reduced blanking: H blank 160 (48/32/80), V front 3, V sync 5,
    /// минимальное время вертикального бланка 460 мкс.
    #[must_use]
    pub fn cvt_rb(&self) -> Timing {
        const H_BLANK: u32 = 160;
        const V_FRONT: u32 = 3;
        const V_SYNC: u32 = 5;
        const V_BACK_MIN: u32 = 6;
        let hz = u64::from(self.refresh_hz.max(1));
        let h = u64::from(self.height);
        // Строк бланка, чтобы вертикальный бланк длился ≥ 460 мкс:
        // vblank ≥ 460e-6 · hz · (h + vblank)  ⇒  vblank ≥ 460·hz·h / (1e6 − 460·hz).
        let denom = 1_000_000_u64.saturating_sub(460 * hz).max(1);
        let min = (460 * hz * h).div_ceil(denom);
        #[allow(clippy::cast_possible_truncation, reason = "строк бланка — сотни")]
        let v_blank = (min as u32).max(V_FRONT + V_SYNC + V_BACK_MIN);
        let total_h = u64::from(self.width + H_BLANK);
        let total_v = u64::from(self.height + v_blank);
        let clock_khz = (total_h * total_v * hz).div_ceil(1000);
        Timing {
            pixel_clock_khz: u32::try_from(clock_khz).unwrap_or(u32::MAX),
            h_blank: H_BLANK,
            h_front: 48,
            h_sync: 32,
            v_blank,
            v_front: V_FRONT,
            v_sync: V_SYNC,
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "поля EDID — байты из 12/10-битных значений"
)]
fn dtd(mode: &DisplayMode, t: &Timing) -> [u8; 18] {
    let clock = (t.pixel_clock_khz.min(MAX_DTD_PIXEL_CLOCK_KHZ) / 10) as u16;
    // Физический размер при 160 dpi (типично для телефона, влияет только на подсказку DPI).
    let mm = |px: u32| (px * 254 / 1600).min(4095);
    let (hmm, vmm) = (mm(mode.width), mm(mode.height));
    let (w, h) = (mode.width, mode.height);
    let [c0, c1] = clock.to_le_bytes();
    [
        c0,
        c1,
        w as u8,
        t.h_blank as u8,
        (((w >> 8) & 0xF) << 4 | ((t.h_blank >> 8) & 0xF)) as u8,
        h as u8,
        t.v_blank as u8,
        (((h >> 8) & 0xF) << 4 | ((t.v_blank >> 8) & 0xF)) as u8,
        t.h_front as u8,
        t.h_sync as u8,
        ((t.v_front & 0xF) << 4 | (t.v_sync & 0xF)) as u8,
        (((t.h_front >> 8) & 3) << 6
            | ((t.h_sync >> 8) & 3) << 4
            | ((t.v_front >> 4) & 3) << 2
            | ((t.v_sync >> 4) & 3)) as u8,
        hmm as u8,
        vmm as u8,
        (((hmm >> 8) & 0xF) << 4 | ((vmm >> 8) & 0xF)) as u8,
        0,
        0,
        // Цифровая раздельная синхронизация, HSync+, VSync− (CVT-RB).
        0x1A,
    ]
}

fn text_descriptor(tag: u8, text: &str) -> [u8; 18] {
    let mut d = [0_u8; 18];
    d[3] = tag;
    let bytes = text.as_bytes();
    for (i, slot) in d[5..].iter_mut().enumerate() {
        *slot = match bytes.get(i) {
            Some(&b) => b,
            None if i == bytes.len() => b'\n',
            None => b' ',
        };
    }
    d
}

/// Блок EDID 1.4 (128 байт) для режима.
#[must_use]
pub fn generate(mode: &DisplayMode) -> [u8; 128] {
    let t = mode.cvt_rb();
    let mut e = [0_u8; 128];
    e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Производитель "RDR": по 5 бит на букву (A = 1), big-endian.
    let id = |c: u8| u16::from(c - b'A' + 1);
    let mfg = (id(b'R') << 10) | (id(b'D') << 5) | id(b'R');
    e[8..10].copy_from_slice(&mfg.to_be_bytes());
    e[10..12].copy_from_slice(&0x0001_u16.to_le_bytes()); // код продукта
    e[12..16].copy_from_slice(&1_u32.to_le_bytes()); // серийный номер
    e[16] = 1; // неделя
    e[17] = 36; // год − 1990 = 2026
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    e[20] = 0xA5; // цифровой вход, 8 бит на канал, DisplayPort
    e[21] = 0; // размер в см не задан (есть в DTD)
    e[22] = 0;
    e[23] = 0x78; // гамма 2.2
    e[24] = 0x06; // sRGB, предпочтительный режим в DTD1
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // Стандартные тайминги не используются.
    for i in 0..8 {
        e[38 + i * 2] = 0x01;
        e[39 + i * 2] = 0x01;
    }
    e[54..72].copy_from_slice(&dtd(mode, &t));
    e[72..90].copy_from_slice(&text_descriptor(0xFC, "RustDroid"));
    // Пределы: вертикаль 24..max(частота) Гц — сообщает ядру, что режим допустим.
    let mut limits = [0_u8; 18];
    limits[3] = 0xFD;
    limits[5] = 24;
    #[allow(clippy::cast_possible_truncation, reason = "частота ≤ 255 Гц")]
    {
        limits[6] = mode.refresh_hz.clamp(24, 255) as u8;
    }
    limits[7] = 1;
    limits[8] = 255;
    #[allow(clippy::cast_possible_truncation, reason = "единицы 10 МГц")]
    {
        limits[9] = (t.pixel_clock_khz.div_ceil(10_000)).min(255) as u8;
    }
    limits[10] = 0x01; // без дополнительных таймингов
    limits[11] = 0x0A;
    for b in &mut limits[12..] {
        *b = 0x20;
    }
    e[90..108].copy_from_slice(&limits);
    e[108..126].copy_from_slice(&text_descriptor(0xFF, "RD-0001"));
    e[126] = 0; // расширений нет
    e[127] = e[..127]
        .iter()
        .fold(0_u8, |a, b| a.wrapping_add(*b))
        .wrapping_neg();
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_and_header() {
        let e = generate(&DisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 144,
        });
        assert_eq!(&e[..8], &[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]);
        assert_eq!(e.iter().fold(0_u8, |a, b| a.wrapping_add(*b)), 0);
        assert_eq!(&e[8..10], &[0x48, 0x92]); // "RDR"
    }

    #[test]
    fn cvt_rb_1080p144() {
        let t = DisplayMode {
            width: 1920,
            height: 1080,
            refresh_hz: 144,
        }
        .cvt_rb();
        // vblank ≈ ceil(460·144·1080 / (1e6 − 460·144)) = 77
        assert_eq!(t.v_blank, 77);
        let refresh = f64::from(t.pixel_clock_khz) * 1000.0 / f64::from((1920 + 160) * (1080 + 77));
        assert!((refresh - 144.0).abs() < 0.1, "{refresh}");
    }

    #[test]
    fn dtd_encodes_mode_and_refresh() {
        let mode = DisplayMode {
            width: 2400,
            height: 1080,
            refresh_hz: 120,
        };
        let e = generate(&mode);
        let d = &e[54..72];
        let clock_khz = u32::from(u16::from_le_bytes([d[0], d[1]])) * 10;
        let hact = u32::from(d[2]) | (u32::from(d[4] >> 4) << 8);
        let hblank = u32::from(d[3]) | (u32::from(d[4] & 0xF) << 8);
        let vact = u32::from(d[5]) | (u32::from(d[7] >> 4) << 8);
        let vblank = u32::from(d[6]) | (u32::from(d[7] & 0xF) << 8);
        assert_eq!((hact, vact), (2400, 1080));
        let hz = f64::from(clock_khz) * 1000.0 / f64::from((hact + hblank) * (vact + vblank));
        assert!((hz - 120.0).abs() < 0.2, "{hz}");
        assert_eq!(e[90 + 6], 120);
    }

    #[test]
    fn low_refresh_has_minimum_blanking() {
        let t = DisplayMode {
            width: 640,
            height: 480,
            refresh_hz: 30,
        }
        .cvt_rb();
        assert!(t.v_blank >= 14);
    }
}
