//! `rustdroid boot`: ВМ с консолью гостя в терминале Windows.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use rustdroid_devices::Serial16550;
use rustdroid_vmm::{RunOutcome, Stopper, Vm, VmConfig};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
};

use crate::cli::BootArgs;

/// Префикс управляющих комбинаций (как в QEMU): Ctrl+A.
const ESCAPE: u8 = 0x01;

/// Вывод консоли гостя: терминал и, опционально, файл.
struct SerialOut {
    log: Option<File>,
}

impl Write for SerialOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut out = std::io::stdout().lock();
        out.write_all(buf)?;
        out.flush()?;
        if let Some(f) = self.log.as_mut() {
            f.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.log.as_mut() {
            f.flush()?;
        }
        std::io::stdout().flush()
    }
}

/// Сырой режим консоли на время работы ВМ; при удалении режимы восстанавливаются.
struct RawConsole {
    input: Option<(HANDLE, CONSOLE_MODE)>,
    output: Option<(HANDLE, CONSOLE_MODE)>,
}

impl RawConsole {
    fn enable() -> Self {
        let input = switch_mode(STD_INPUT_HANDLE, |m| {
            (m & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT
        });
        let output = switch_mode(STD_OUTPUT_HANDLE, |m| {
            m | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        });
        Self { input, output }
    }
}

fn switch_mode(
    which: windows::Win32::System::Console::STD_HANDLE,
    f: impl FnOnce(CONSOLE_MODE) -> CONSOLE_MODE,
) -> Option<(HANDLE, CONSOLE_MODE)> {
    // SAFETY: получение стандартного дескриптора процесса.
    let handle = unsafe { GetStdHandle(which) }.ok()?;
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: дескриптор консоли и выходной указатель валидны; если это не консоль
    // (перенаправление в файл), вызов вернёт ошибку и режим не меняется.
    unsafe { GetConsoleMode(handle, &raw mut mode) }.ok()?;
    // SAFETY: дескриптор консоли валиден.
    unsafe { SetConsoleMode(handle, f(mode)) }.ok()?;
    Some((handle, mode))
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        for (handle, mode) in [self.input.take(), self.output.take()]
            .into_iter()
            .flatten()
        {
            // SAFETY: восстанавливаем исходный режим того же дескриптора.
            let _ = unsafe { SetConsoleMode(handle, mode) };
        }
    }
}

/// Поток ввода: клавиатура → COM1 гостя. `Ctrl+A X` останавливает ВМ, `Ctrl+A Ctrl+A` — передаёт Ctrl+A.
fn spawn_input(serial: Arc<Mutex<Serial16550>>, stopper: Stopper) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("console-input".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0_u8; 256];
            let mut escape = false;
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let mut pending = Vec::with_capacity(n);
                for &b in buf.get(..n).unwrap_or(&[]) {
                    if escape {
                        escape = false;
                        match b {
                            b'x' | b'X' => {
                                stopper.stop();
                                return;
                            }
                            ESCAPE => pending.push(ESCAPE),
                            _ => {}
                        }
                    } else if b == ESCAPE {
                        escape = true;
                    } else {
                        pending.push(b);
                    }
                }
                let mut rest = pending.as_slice();
                while !rest.is_empty() {
                    let accepted = serial
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .enqueue_input(rest);
                    rest = rest.get(accepted..).unwrap_or(&[]);
                    if !rest.is_empty() {
                        // FIFO гостя заполнен — ждём, пока драйвер его вычитает.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        })?;
    Ok(())
}

/// Выполняет `rustdroid boot`.
pub(crate) fn boot(args: &BootArgs) -> Result<()> {
    let log = args
        .serial_log
        .as_ref()
        .map(|p| File::create(p).with_context(|| format!("не удалось создать {}", p.display())))
        .transpose()?;

    let cfg = VmConfig {
        kernel: args.kernel.clone(),
        initrd: args.initrd.clone(),
        memory_mib: args.memory,
        cmdline_extra: args.append.clone(),
        cmdline_override: args.cmdline.clone(),
        large_pages: args.large_pages,
        guest_hz: args.guest_hz,
    };
    let vm = Vm::new(&cfg, Box::new(SerialOut { log })).context("не удалось создать ВМ")?;
    tracing::info!(cmdline = vm.cmdline(), "командная строка ядра");
    tracing::info!("Консоль гостя ниже. Выход: Ctrl+A, затем X.");

    let _raw = RawConsole::enable();
    spawn_input(vm.serial(), vm.stopper()).context("не удалось запустить поток ввода")?;
    let (outcome, stats) = vm.run().context("ошибка выполнения ВМ")?;
    drop(_raw);
    let _ = writeln!(std::io::stdout());
    match outcome {
        RunOutcome::Reset => {
            tracing::info!(exits = stats.total(), "гость перезагрузился или выключился")
        }
        RunOutcome::Stopped => {
            tracing::info!(exits = stats.total(), "ВМ остановлена пользователем")
        }
    }
    Ok(())
}
