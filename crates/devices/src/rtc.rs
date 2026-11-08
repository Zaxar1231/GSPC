//! Часы реального времени CMOS (MC146818): порты `0x70` (индекс) и `0x71` (данные).
//!
//! Без них ядро при загрузке десятки тысяч раз читает `0xFF` из порта `0x71` (флаг «идёт
//! обновление» никогда не сбрасывается), а гость не знает текущего времени. Время отдаётся в
//! BCD, 24-часовом формате, UTC. Флаг UIP всегда сброшен, запись в часы игнорируется (время
//! гостя корректирует сам Android по сети), остальные байты — обычная NVRAM.

use std::time::{SystemTime, UNIX_EPOCH};

/// Порт индекса.
pub const RTC_INDEX_PORT: u16 = 0x70;
/// Порт данных.
pub const RTC_DATA_PORT: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_WEEKDAY: u8 = 0x06;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_A: u8 = 0x0A;
const REG_B: u8 = 0x0B;
const REG_C: u8 = 0x0C;
const REG_D: u8 = 0x0D;
const REG_CENTURY: u8 = 0x32;

/// Регистр A: делитель 32,768 кГц, частота 1024 Гц, UIP = 0.
const REG_A_VALUE: u8 = 0x26;
/// Регистр B: 24-часовой формат, BCD, прерывания выключены.
const REG_B_VALUE: u8 = 0x02;
/// Регистр D: батарея в порядке (VRT).
const REG_D_VALUE: u8 = 0x80;

/// Дата и время UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    /// Год (например, 2026).
    pub year: u32,
    /// Месяц 1..=12.
    pub month: u8,
    /// День 1..=31.
    pub day: u8,
    /// День недели 1..=7 (1 — воскресенье, как в CMOS).
    pub weekday: u8,
    /// Часы 0..=23.
    pub hour: u8,
    /// Минуты.
    pub minute: u8,
    /// Секунды.
    pub second: u8,
}

impl DateTime {
    /// Перевод секунд Unix-времени в календарную дату (алгоритм Хиннанта, григорианский
    /// календарь).
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "значения ограничены календарём"
    )]
    pub fn from_unix(secs: u64) -> Self {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as u32;
        // 1970-01-01 — четверг (5 в нумерации CMOS, где 1 — воскресенье).
        let weekday = ((days + 4).rem_euclid(7) + 1) as u8;
        Self {
            year,
            month,
            day,
            weekday,
            hour: (rem / 3600) as u8,
            minute: (rem / 60 % 60) as u8,
            second: (rem % 60) as u8,
        }
    }
}

fn bcd(v: u32) -> u8 {
    #[allow(clippy::cast_possible_truncation, reason = "две десятичные цифры")]
    let v = (v % 100) as u8;
    ((v / 10) << 4) | (v % 10)
}

/// Модель CMOS RTC.
#[derive(Debug)]
pub struct CmosRtc {
    index: u8,
    nvram: [u8; 128],
    now: fn() -> u64,
}

impl Default for CmosRtc {
    fn default() -> Self {
        Self::with_clock(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        })
    }
}

impl CmosRtc {
    /// RTC с заданным источником времени (секунды Unix).
    #[must_use]
    pub fn with_clock(now: fn() -> u64) -> Self {
        Self {
            index: 0,
            nvram: [0; 128],
            now,
        }
    }

    /// Чтение порта `0x70`/`0x71`.
    #[must_use]
    pub fn read(&self, port: u16) -> u8 {
        if port != RTC_DATA_PORT {
            return 0xFF;
        }
        let t = DateTime::from_unix((self.now)());
        match self.index {
            REG_SECONDS => bcd(u32::from(t.second)),
            REG_MINUTES => bcd(u32::from(t.minute)),
            REG_HOURS => bcd(u32::from(t.hour)),
            REG_WEEKDAY => bcd(u32::from(t.weekday)),
            REG_DAY => bcd(u32::from(t.day)),
            REG_MONTH => bcd(u32::from(t.month)),
            REG_YEAR => bcd(t.year % 100),
            REG_CENTURY => bcd(t.year / 100),
            REG_A => REG_A_VALUE,
            REG_B => REG_B_VALUE,
            REG_C => 0,
            REG_D => REG_D_VALUE,
            i => self.nvram.get(usize::from(i)).copied().unwrap_or(0xFF),
        }
    }

    /// Запись в порт `0x70`/`0x71`.
    pub fn write(&mut self, port: u16, value: u8) {
        match port {
            // Бит 7 — маска NMI, индекс — младшие 7 бит.
            RTC_INDEX_PORT => self.index = value & 0x7F,
            RTC_DATA_PORT if self.index > REG_D && self.index != REG_CENTURY => {
                if let Some(b) = self.nvram.get_mut(usize::from(self.index)) {
                    *b = value;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(rtc: &mut CmosRtc, index: u8) -> u8 {
        rtc.write(RTC_INDEX_PORT, index | 0x80);
        rtc.read(RTC_DATA_PORT)
    }

    #[test]
    fn unix_to_calendar() {
        let epoch = DateTime::from_unix(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!(epoch.weekday, 5);
        // 2026-11-09 00:25:42 UTC, понедельник.
        let t = DateTime::from_unix(1_794_183_942);
        assert_eq!(
            t,
            DateTime {
                year: 2026,
                month: 11,
                day: 9,
                weekday: 2,
                hour: 0,
                minute: 25,
                second: 42
            }
        );
        // 29 февраля високосного 2024 года, 23:59:59.
        let leap = DateTime::from_unix(1_709_251_199);
        assert_eq!((leap.year, leap.month, leap.day), (2024, 2, 29));
        assert_eq!((leap.hour, leap.minute, leap.second), (23, 59, 59));
    }

    #[test]
    fn registers_in_bcd() {
        let mut rtc = CmosRtc::with_clock(|| 1_794_183_942);
        assert_eq!(reg(&mut rtc, REG_SECONDS), 0x42);
        assert_eq!(reg(&mut rtc, REG_MINUTES), 0x25);
        assert_eq!(reg(&mut rtc, REG_HOURS), 0x00);
        assert_eq!(reg(&mut rtc, REG_DAY), 0x09);
        assert_eq!(reg(&mut rtc, REG_MONTH), 0x11);
        assert_eq!(reg(&mut rtc, REG_YEAR), 0x26);
        assert_eq!(reg(&mut rtc, REG_CENTURY), 0x20);
        assert_eq!(reg(&mut rtc, REG_A) & 0x80, 0, "UIP сброшен");
        assert_eq!(reg(&mut rtc, REG_B), REG_B_VALUE);
        assert_eq!(reg(&mut rtc, REG_D), REG_D_VALUE);
    }

    #[test]
    fn nvram_keeps_values_and_clock_ignores_writes() {
        let mut rtc = CmosRtc::with_clock(|| 0);
        rtc.write(RTC_INDEX_PORT, 0x40);
        rtc.write(RTC_DATA_PORT, 0x5A);
        assert_eq!(reg(&mut rtc, 0x40), 0x5A);
        rtc.write(RTC_INDEX_PORT, REG_SECONDS);
        rtc.write(RTC_DATA_PORT, 0x33);
        assert_eq!(reg(&mut rtc, REG_SECONDS), 0x00);
        assert_eq!(rtc.read(RTC_INDEX_PORT), 0xFF);
    }
}
