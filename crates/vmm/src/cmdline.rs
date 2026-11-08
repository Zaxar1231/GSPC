//! Командная строка ядра по умолчанию.
//!
//! Платформа hardware-reduced (без PIT/HPET/PM-таймера), поэтому калибровку таймеров ядру
//! подсказываем значениями, которые сообщает гипервизор:
//! - `tsc_early_khz` — частота TSC, ядро пропускает калибровку;
//! - `lapic_timer_period` — тиков таймера LAPIC на jiffy;
//! - `lpj` — loops_per_jiffy для задержек (TSC-цикл на jiffy).

/// Частоты, полученные от гипервизора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Частота TSC гостя, Гц.
    pub tsc_hz: u64,
    /// Частота таймера LAPIC, Гц.
    pub apic_hz: u64,
    /// `CONFIG_HZ` ядра гостя (GKI x86_64 — 250).
    pub guest_hz: u32,
}

/// Собирает командную строку: базовые параметры платформы + `extra` пользователя.
#[must_use]
pub fn build(timing: &Timing, extra: &str) -> String {
    let hz = u64::from(timing.guest_hz.max(1));
    let mut parts = vec![
        "console=ttyS0,115200".to_owned(),
        "earlycon=uart8250,io,0x3f8,115200".to_owned(),
        // В GKI CONFIG_SERIAL_8250_RUNTIME_UARTS=0: без этого ttyS0 не регистрируется,
        // /dev/console указывает на ttynull и вывод программ гостя теряется.
        "8250.nr_uarts=1".to_owned(),
        format!("tsc_early_khz={}", timing.tsc_hz / 1000),
        "tsc=reliable".to_owned(),
        format!("lpj={}", timing.tsc_hz / hz),
        format!("lapic_timer_period={}", timing.apic_hz / hz),
        "no_timer_check".to_owned(),
        "reboot=k".to_owned(),
        "panic=-1".to_owned(),
        "i8042.noaux".to_owned(),
        "i8042.nomux".to_owned(),
        "i8042.nopnp".to_owned(),
        "i8042.dumbkbd".to_owned(),
    ];
    let extra = extra.trim();
    if !extra.is_empty() {
        parts.push(extra.to_owned());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_timing_parameters() {
        let t = Timing {
            tsc_hz: 3_000_000_000,
            apic_hz: 1_000_000_000,
            guest_hz: 250,
        };
        let c = build(&t, "  loglevel=7 ");
        assert!(c.contains("tsc_early_khz=3000000"));
        assert!(c.contains("8250.nr_uarts=1"));
        assert!(c.contains("lpj=12000000"));
        assert!(c.contains("lapic_timer_period=4000000"));
        assert!(c.ends_with(" loglevel=7"));
        assert!(c.starts_with("console=ttyS0"));
    }

    #[test]
    fn zero_hz_does_not_divide_by_zero() {
        let t = Timing {
            tsc_hz: 1000,
            apic_hz: 1000,
            guest_hz: 0,
        };
        assert!(build(&t, "").contains("lpj=1000"));
    }
}
