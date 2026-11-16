//! Доставка гостевого агента через консоль гостя (`--agent PATH`): без правок образа.
//!
//! Скрипт шлёт бинарник в shell гостя кусками base64 в `/data/local/tmp`, раскодирует, даёт
//! право на запуск и стартует агент в фоне. Работает только при правах оболочки, которые
//! позволяют писать в `/data/local/tmp` и запускать бинарники; на железе пока не проверено.

/// Размер куска base64 в одной команде shell (короче предела строки).
const CHUNK: usize = 3000;
/// Куда кладём агента в госте.
pub(crate) const GUEST_PATH: &str = "/data/local/tmp/rustdroid-agent";

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Кодирует `data` в base64 (стандартный алфавит, с дополнением `=`).
#[must_use]
pub(crate) fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let idx = usize::try_from((n >> (18 - 6 * i)) & 63).unwrap_or(0);
                out.push(char::from(ALPHABET[idx]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Скрипт shell: записать `bin` в [`GUEST_PATH`] и запустить. Строка `rustdroid: agent …`
/// в консоли говорит, как прошло.
#[must_use]
pub(crate) fn push_script(bin: &[u8]) -> String {
    let b64 = base64(bin);
    let tmp = format!("{GUEST_PATH}.b64");
    // Без эха: терминал гостя повторяет каждую строку обратно в консоль, и ~1 МБ base64 шёл
    // вдвое дольше (за 4 мин дошло 237 кусков из 327). Эхо выключается первой строкой — до
    // прихода основного объёма в буфер терминала.
    let mut s = String::from(
        "stty -echo\nwhile [ \"$(getprop sys.boot_completed)\" != 1 ]; do sleep 2; done\n",
    );
    s.push_str(&format!("rm -f {tmp}\n"));
    for part in b64.as_bytes().chunks(CHUNK) {
        // base64 — только ASCII, безопасно в одинарных кавычках.
        let text = std::str::from_utf8(part).unwrap_or("");
        s.push_str(&format!("printf '%s' '{text}' >> {tmp}\n"));
    }
    s.push_str(&format!(
        "base64 -d {tmp} > {GUEST_PATH} && chmod 755 {GUEST_PATH} && rm -f {tmp} && \
         echo 'rustdroid: agent written' || echo 'rustdroid: agent write failed'\n"
    ));
    s.push_str(&format!(
        "(nohup {GUEST_PATH} > {GUEST_PATH}.log 2>&1 &) ; echo 'rustdroid: agent started'; stty echo\n"
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn script_splits_long_payload_into_chunks() {
        let bin = vec![0xAB; 10_000];
        let script = push_script(&bin);
        let appends = script.matches(" >> ").count();
        assert_eq!(appends, base64(&bin).len().div_ceil(CHUNK));
        assert!(script.contains("chmod 755"));
        assert!(script.starts_with("stty -echo\n"));
        assert!(script.contains("'rustdroid: agent started'"));
    }
}
