//! Загрузка CPU и память процесса эмулятора для оверлея и логов.
//!
//! Здесь только арифметика над счётчиками ОС (тестируется на любой платформе); сами счётчики
//! (`GetProcessTimes`, `GetSystemTimes`, `GetProcessMemoryInfo`) читает композитор.

/// Снимок счётчиков (времена — в единицах по 100 нс, как в `FILETIME`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageSample {
    /// Время на стене.
    pub wall: u64,
    /// Время CPU процесса (ядро + пользователь, все потоки).
    pub process: u64,
    /// Простой системы (все логические процессоры).
    pub system_idle: u64,
    /// Время ядра системы (включает простой).
    pub system_kernel: u64,
    /// Пользовательское время системы.
    pub system_user: u64,
    /// Рабочий набор процесса, байты.
    pub working_set: u64,
    /// Суммарное время работы vCPU гостя (счётчики гипервизора), если известно.
    pub guest_runtime: Option<u64>,
}

/// Загрузка за интервал между двумя снимками.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Доля всех логических процессоров, занятая процессом, %.
    pub process_cpu: f32,
    /// Загрузка CPU всей системы, %.
    pub system_cpu: f32,
    /// Рабочий набор процесса, ГиБ.
    pub working_set_gib: f32,
    /// Средняя загрузка vCPU гостя, % (100 — все vCPU заняты всё время).
    pub guest_cpu: Option<f32>,
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "проценты и гибибайты для отображения"
)]
impl Usage {
    /// Загрузка между снимками `prev` и `now` при `cpus` логических процессорах.
    #[must_use]
    pub fn between(prev: &UsageSample, now: &UsageSample, cpus: u32, vcpus: u32) -> Self {
        let wall = now.wall.saturating_sub(prev.wall);
        let proc = now.process.saturating_sub(prev.process);
        let process_cpu = if wall == 0 {
            0.0
        } else {
            (proc as f64 / (wall as f64 * f64::from(cpus.max(1))) * 100.0) as f32
        };
        let idle = now.system_idle.saturating_sub(prev.system_idle);
        let total = now.system_kernel.saturating_sub(prev.system_kernel)
            + now.system_user.saturating_sub(prev.system_user);
        let system_cpu = if total == 0 {
            0.0
        } else {
            (total.saturating_sub(idle) as f64 / total as f64 * 100.0) as f32
        };
        let guest_cpu = match (prev.guest_runtime, now.guest_runtime) {
            (Some(a), Some(b)) if wall > 0 => Some(
                (b.saturating_sub(a) as f64 / (wall as f64 * f64::from(vcpus.max(1))) * 100.0)
                    .clamp(0.0, 100.0) as f32,
            ),
            _ => None,
        };
        Self {
            guest_cpu,
            process_cpu: process_cpu.clamp(0.0, 100.0),
            system_cpu: system_cpu.clamp(0.0, 100.0),
            working_set_gib: (now.working_set as f64 / f64::from(1_u32 << 30)) as f32,
        }
    }

    /// Строки оверлея.
    #[must_use]
    pub fn overlay_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("RAM {:.1} GB", self.working_set_gib)];
        if let Some(g) = self.guest_cpu {
            lines.push(format!("VCPU {g:.0}%"));
        }
        lines.push(format!(
            "CPU {:.0}% ALL {:.0}%",
            self.process_cpu, self.system_cpu
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_and_system_load() {
        let prev = UsageSample::default();
        // 1 с на стене, 8 процессоров; процесс занял 2 с CPU (= 25 %), система — 4 из 8 с.
        let now = UsageSample {
            wall: 10_000_000,
            process: 20_000_000,
            system_idle: 40_000_000,
            system_kernel: 50_000_000,
            system_user: 30_000_000,
            working_set: 4_617_089_843,
            guest_runtime: None,
        };
        let u = Usage::between(&prev, &now, 8, 4);
        assert!((u.process_cpu - 25.0).abs() < 0.01);
        assert!((u.system_cpu - 50.0).abs() < 0.01);
        assert!((u.working_set_gib - 4.3).abs() < 0.01);
        assert_eq!(u.overlay_lines(), ["RAM 4.3 GB", "CPU 25% ALL 50%"]);
    }

    #[test]
    fn guest_vcpu_load() {
        let prev = UsageSample {
            guest_runtime: Some(0),
            ..UsageSample::default()
        };
        // 1 с, 4 vCPU отработали в сумме 3 с — 75 %.
        let now = UsageSample {
            wall: 10_000_000,
            guest_runtime: Some(30_000_000),
            ..UsageSample::default()
        };
        let u = Usage::between(&prev, &now, 8, 4);
        assert_eq!(u.guest_cpu, Some(75.0));
        assert_eq!(u.overlay_lines()[1], "VCPU 75%");
    }

    #[test]
    fn empty_interval_is_zero() {
        let s = UsageSample::default();
        let u = Usage::between(&s, &s, 0, 0);
        assert_eq!((u.process_cpu, u.system_cpu), (0.0, 0.0));
    }
}
