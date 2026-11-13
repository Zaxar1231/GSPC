//! Загрузка CPU по группам потоков процесса: где эмулятор тратит процессор.
//!
//! Группа — имя потока без номера в конце (`vcpu0`…`vcpu3` → `vcpu`); потоки без имени (gfxstream,
//! драйвер видеокарты, пул Windows) попадают в группу [`UNNAMED`]. Здесь только арифметика;
//! снимки (`GetThreadTimes`, `GetThreadDescription`) делает платформенный код.

use std::collections::HashMap;

/// Группа потоков без имени.
pub const UNNAMED: &str = "без имени";

/// Снимок одного потока.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSample {
    /// Идентификатор потока.
    pub id: u32,
    /// Имя потока (пустое — без имени).
    pub name: String,
    /// Время CPU потока (ядро + пользователь), единицы по 100 нс.
    pub cpu: u64,
}

/// Загрузка одной группы за интервал.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupLoad {
    /// Имя группы.
    pub group: String,
    /// Потоков в группе (в последнем снимке).
    pub threads: usize,
    /// Доля всех логических процессоров, %.
    pub cpu_pct: f32,
}

/// Имя группы для имени потока.
#[must_use]
pub fn group_of(name: &str) -> &str {
    let trimmed = name.trim();
    let base = trimmed.trim_end_matches(|c: char| c.is_ascii_digit());
    let base = base.trim_end_matches(['-', '_', ' ', '#']);
    if base.is_empty() {
        if trimmed.is_empty() { UNNAMED } else { trimmed }
    } else {
        base
    }
}

/// Загрузка групп между снимками `prev` и `now` за `wall` (100 нс) при `cpus` процессорах,
/// по убыванию. Поток, которого не было в `prev`, считается целиком (он появился за интервал).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "проценты для лога"
)]
pub fn group_load(
    prev: &[ThreadSample],
    now: &[ThreadSample],
    wall: u64,
    cpus: u32,
) -> Vec<GroupLoad> {
    let before: HashMap<u32, u64> = prev.iter().map(|t| (t.id, t.cpu)).collect();
    let mut groups: HashMap<&str, (usize, u64)> = HashMap::new();
    for t in now {
        let spent = t
            .cpu
            .saturating_sub(before.get(&t.id).copied().unwrap_or(0));
        let g = groups.entry(group_of(&t.name)).or_default();
        g.0 += 1;
        g.1 += spent;
    }
    let total = wall as f64 * f64::from(cpus.max(1));
    let mut out: Vec<GroupLoad> = groups
        .into_iter()
        .map(|(group, (threads, spent))| GroupLoad {
            group: group.to_owned(),
            threads,
            cpu_pct: if total > 0.0 {
                (spent as f64 / total * 100.0) as f32
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by(|a, b| {
        b.cpu_pct
            .total_cmp(&a.cpu_pct)
            .then_with(|| a.group.cmp(&b.group))
    });
    out
}

/// Строка для лога: `vcpu×4 61.2%, без имени×23 20.4%, …` (группы от 0,5 %).
#[must_use]
pub fn format_loads(loads: &[GroupLoad]) -> String {
    let shown: Vec<String> = loads
        .iter()
        .filter(|l| l.cpu_pct >= 0.5)
        .map(|l| format!("{}×{} {:.1}%", l.group, l.threads, l.cpu_pct))
        .collect();
    if shown.is_empty() {
        "нет нагрузки".to_owned()
    } else {
        shown.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: u32, name: &str, cpu: u64) -> ThreadSample {
        ThreadSample {
            id,
            name: name.to_owned(),
            cpu,
        }
    }

    #[test]
    fn groups_strip_numbers() {
        assert_eq!(group_of("vcpu3"), "vcpu");
        assert_eq!(group_of("virtio-gpu"), "virtio-gpu");
        assert_eq!(group_of("worker-12"), "worker");
        assert_eq!(group_of(""), UNNAMED);
        assert_eq!(group_of("  "), UNNAMED);
        assert_eq!(group_of("42"), "42");
    }

    #[test]
    fn loads_by_group() {
        // 1 с, 4 процессора = 4 с CPU всего.
        let prev = [t(1, "vcpu0", 0), t(2, "vcpu1", 10), t(3, "", 0)];
        let now = [
            t(1, "vcpu0", 10_000_000),
            t(2, "vcpu1", 10_000_010),
            t(3, "", 2_000_000),
            t(4, "", 2_000_000), // новый поток
            t(5, "present", 0),
        ];
        let l = group_load(&prev, &now, 10_000_000, 4);
        assert_eq!(l[0].group, "vcpu");
        assert_eq!(l[0].threads, 2);
        assert!((l[0].cpu_pct - 50.0).abs() < 0.01);
        assert_eq!(l[1].group, UNNAMED);
        assert!((l[1].cpu_pct - 10.0).abs() < 0.01);
        assert_eq!(l[2].group, "present");
        assert_eq!(format_loads(&l), "vcpu×2 50.0%, без имени×2 10.0%");
        assert_eq!(format_loads(&[]), "нет нагрузки");
        assert!(
            group_load(&prev, &now, 0, 4)
                .iter()
                .all(|g| g.cpu_pct == 0.0)
        );
    }
}
