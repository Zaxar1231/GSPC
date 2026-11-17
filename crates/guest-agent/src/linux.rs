//! Linux-часть агента: сокет vsock, команды Android (`dumpsys`, `pm`).

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::Command;
use std::time::{Duration, Instant};

use rustdroid_agent::proto::{Decoder, Message, PORT, VERSION, encode};

use crate::foreground_package;

/// CID хоста в vsock.
const HOST_CID: u32 = 2;
/// Как часто опрашивать активное приложение.
const FOREGROUND_EVERY: Duration = Duration::from_secs(1);

#[repr(C)]
struct SockaddrVm {
    family: u16,
    reserved: u16,
    port: u32,
    cid: u32,
    zero: [u8; 4],
}

/// Подключается к хосту и возвращает сокет.
fn connect_host() -> std::io::Result<File> {
    // SAFETY: создание сокета с известными аргументами; результат проверяется.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd только что создан и не используется в других местах.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let addr = SockaddrVm {
        family: u16::try_from(libc::AF_VSOCK).unwrap_or(40),
        reserved: 0,
        port: PORT,
        cid: HOST_CID,
        zero: [0; 4],
    };
    #[allow(clippy::cast_possible_truncation, reason = "размер структуры мал")]
    let len = std::mem::size_of::<SockaddrVm>() as libc::socklen_t;
    // SAFETY: адрес — локальная структура правильного размера и раскладки (sockaddr_vm).
    let r = unsafe {
        libc::connect(
            std::os::fd::AsRawFd::as_raw_fd(&owned),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(File::from(owned))
}

/// Переводит сокет в неблокирующий режим (`O_NONBLOCK`).
fn set_nonblocking(sock: &File) -> std::io::Result<()> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(sock);
    // SAFETY: fd принадлежит открытому файлу, аргументы F_GETFL/F_SETFL без указателей.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: см. выше; меняем только флаг O_NONBLOCK.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        })
        .unwrap_or_default()
}

/// Установка, которая собирается из кусков.
struct Install {
    name: String,
    expected: u64,
    file: File,
    path: std::path::PathBuf,
    written: u64,
}

fn finish_install(id: u32, inst: Install) -> Message {
    let _ = inst.file.sync_all();
    drop(inst.file);
    let ok_size = inst.written == inst.expected;
    let (ok, message) = if ok_size {
        let out = sh("pm", &["install", "-r", &inst.path.to_string_lossy()]);
        (out.contains("Success"), out.trim().to_owned())
    } else {
        (
            false,
            format!("получено {} из {} байт", inst.written, inst.expected),
        )
    };
    let _ = std::fs::remove_file(&inst.path);
    tracing_note(&inst.name, ok, &message);
    Message::InstallResult { id, ok, message }
}

/// Сообщение в stderr агента (в Android попадает в логи служб).
#[allow(clippy::print_stderr, reason = "агент — демон, журнал — stderr")]
fn tracing_note(name: &str, ok: bool, message: &str) {
    eprintln!(
        "rustdroid-agent: установка {name}: {} ({message})",
        if ok { "ок" } else { "ошибка" }
    );
}

/// Основной цикл: приветствие, активное приложение, приём команд хоста.
pub(crate) fn run() -> Result<(), String> {
    let mut sock = connect_host().map_err(|e| format!("нет связи с хостом: {e}"))?;
    sock.write_all(&encode(&Message::Hello { version: VERSION }))
        .map_err(|e| e.to_string())?;
    set_nonblocking(&sock).map_err(|e| e.to_string())?;
    let mut decoder = Decoder::default();
    let mut install: Option<(u32, Install)> = None;
    let mut last_fg = String::new();
    let mut next_fg = Instant::now();
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0_usize;
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                total += n;
                let msgs = decoder.feed(buf.get(..n).unwrap_or(&[])).map_err(|e| {
                    // Что именно пришло: без этого порчу потока не найти.
                    let head = decoder.buffered();
                    let hex: Vec<String> =
                        head.iter().take(24).map(|b| format!("{b:02x}")).collect();
                    format!(
                        "{e} (всего {total} Б, чтение {n} Б, в буфере {} Б: {})",
                        head.len(),
                        hex.join(" ")
                    )
                })?;
                for m in msgs {
                    handle_host(m, &mut sock, &mut install)?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e.to_string()),
        }
        if Instant::now() >= next_fg {
            next_fg = Instant::now() + FOREGROUND_EVERY;
            let fg = foreground_package(&sh("dumpsys", &["activity", "activities"]));
            if fg != last_fg {
                last_fg.clone_from(&fg);
                sock.write_all(&encode(&Message::Foreground { package: fg }))
                    .map_err(|e| e.to_string())?;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn handle_host(
    m: Message,
    sock: &mut File,
    install: &mut Option<(u32, Install)>,
) -> Result<(), String> {
    match m {
        Message::HelloAck { .. } => {}
        Message::Ping { token } => {
            sock.write_all(&encode(&Message::Pong { token }))
                .map_err(|e| e.to_string())?;
        }
        Message::InstallBegin { id, size, name } => {
            let path = std::env::temp_dir().join(format!("rustdroid-install-{id}.apk"));
            let file = File::create(&path).map_err(|e| e.to_string())?;
            *install = Some((
                id,
                Install {
                    name,
                    expected: size,
                    file,
                    path,
                    written: 0,
                },
            ));
        }
        Message::InstallChunk { id, data } => {
            if let Some((cur, inst)) = install.as_mut()
                && *cur == id
            {
                inst.file.write_all(&data).map_err(|e| e.to_string())?;
                inst.written += data.len() as u64;
            }
        }
        Message::InstallEnd { id } => {
            let matches = install.as_ref().is_some_and(|(cur, _)| *cur == id);
            if matches && let Some((_, inst)) = install.take() {
                let reply = finish_install(id, inst);
                sock.write_all(&encode(&reply)).map_err(|e| e.to_string())?;
            }
        }
        _ => {}
    }
    Ok(())
}
