//! Доставка гостевого агента (`--agent PATH`) без правок образа.
//!
//! Бинарник идёт не через shell на `hvc0` (там ~2 КБ/с: строчный редактор и логи ядра), а через
//! отдельную консоль [`DROP_HVC`]: shell гостя открывает её, включает raw-режим и пишет строку;
//! увидев её, хост отдаёт бинарник целиком, гость читает ровно его размер (`head -c`).

use std::io::Write;
use std::sync::Arc;

use rustdroid_devices::ConsoleInput;

/// Номер консоли передачи (после hvc0…hvc19 Cuttlefish).
pub(crate) const DROP_HVC: u32 = 20;
/// Куда кладём агента в госте.
pub(crate) const GUEST_PATH: &str = "/data/local/tmp/rustdroid-agent";

/// Скрипт shell для `hvc0`: принять бинарник размером `size` с [`DROP_HVC`] и запустить агента
/// от root. Работает в фоне, консоль не занимает. Строка `rustdroid: agent …` — итог.
#[must_use]
pub(crate) fn push_script(size: usize) -> String {
    format!(
        "(while [ \"$(getprop sys.boot_completed)\" != 1 ]; do sleep 2; done; \
         su 0 sh -c 'exec 3<>/dev/hvc{DROP_HVC}; stty raw -echo <&3; echo go >&3; \
         head -c {size} <&3 > {GUEST_PATH}; chmod 755 {GUEST_PATH}; \
         [ $(stat -c %s {GUEST_PATH}) = {size} ]' </dev/null >/dev/null 2>&1 \
         && echo 'rustdroid: agent written' || echo 'rustdroid: agent write failed'; \
         su 0 sh -c 'nohup {GUEST_PATH} > {GUEST_PATH}.log 2>&1 &' </dev/null; \
         echo 'rustdroid: agent started'; sleep 10; \
         su 0 head -5 {GUEST_PATH}.log | sed 's/^/AGENTLOG /') &\n"
    )
}

/// Вывод консоли [`DROP_HVC`]: первая строка гостя значит «читаю» — тогда бинарник уходит во
/// ввод консоли (устройство отдаст его, обработав передачу гостя).
pub(crate) struct AgentDrop {
    input: Arc<ConsoleInput>,
    bin: Option<Vec<u8>>,
}

impl AgentDrop {
    pub(crate) fn new(input: Arc<ConsoleInput>, bin: Vec<u8>) -> Self {
        Self {
            input,
            bin: Some(bin),
        }
    }
}

impl Write for AgentDrop {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.contains(&b'\n')
            && let Some(bin) = self.bin.take()
        {
            tracing::info!(bytes = bin.len(), "гость готов принять агента");
            self.input.push(&bin);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_reads_exact_size_from_drop_console() {
        let s = push_script(734_016);
        assert!(s.contains("/dev/hvc20"));
        assert!(s.contains("head -c 734016"));
        assert!(s.contains("'rustdroid: agent started'"));
        assert!(s.ends_with(") &\n"));
    }

    #[test]
    fn binary_is_sent_once_after_guest_line() {
        let mut drop = AgentDrop::new(Arc::default(), b"BIN".to_vec());
        drop.write_all(b"g").unwrap();
        assert!(drop.bin.is_some());
        drop.write_all(b"o\n").unwrap();
        assert!(drop.bin.is_none());
        drop.write_all(b"go\n").unwrap();
    }
}
