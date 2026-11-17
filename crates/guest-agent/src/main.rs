//! Гостевой агент RustDroid.
//!
//! Работает внутри Android (цель `x86_64-linux-android`) и общается с хостом через virtio-vsock
//! (CID 2, порт [`rustdroid_agent::PORT`]): шлёт `Hello`, сообщает активное приложение и
//! устанавливает APK, присланные хостом. Протокол — `rustdroid_agent::proto`.
//!
//! На хосте, кроме Linux и Android, бинарник собирается, но не работает: vsock есть только там.
//! Важно: у цели Android `target_os = "android"`, не `"linux"`.

use std::process::ExitCode;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;

fn main() -> ExitCode {
    // Демон: связь с хостом оборвалась или не установилась — пробуем снова.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    loop {
        let e = linux::run()
            .err()
            .unwrap_or_else(|| "хост закрыл соединение".to_owned());
        #[allow(clippy::print_stderr, reason = "агент — демон, журнал — stderr")]
        {
            eprintln!("rustdroid-agent: {e}; повтор через 2 с");
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        #[allow(clippy::print_stderr, reason = "агент — демон, журнал — stderr")]
        {
            eprintln!("rustdroid-agent работает только в Linux (Android)");
        }
        ExitCode::FAILURE
    }
}

/// Разбор `dumpsys activity activities`: пакет resumed-активности (пусто — не найден).
#[must_use]
pub fn foreground_package(dump: &str) -> String {
    for line in dump.lines() {
        let t = line.trim();
        if !(t.starts_with("mResumedActivity") || t.starts_with("topResumedActivity")) {
            continue;
        }
        // Формат: "... ActivityRecord{hash u0 com.pkg/.Activity t12}".
        if let Some(rest) = t.split(" u0 ").nth(1)
            && let Some(component) = rest.split_whitespace().next()
            && let Some((pkg, _)) = component.split_once('/')
        {
            return pkg.to_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::foreground_package;

    #[test]
    fn parses_resumed_activity() {
        let dump =
            "  mResumedActivity: ActivityRecord{abc123 u0 com.example.game/.MainActivity t42}\n";
        assert_eq!(foreground_package(dump), "com.example.game");
        assert_eq!(foreground_package("  mFocusedApp=null\n"), "");
        let top = "  topResumedActivity=ActivityRecord{1 u0 com.android.launcher3/.Launcher t1}";
        assert_eq!(foreground_package(top), "com.android.launcher3");
    }
}
