//! Сеть гостя на стороне хоста: NAT в пользовательском режиме (`--net user`) и DNS хоста.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustdroid_devices::{NatBackend, NetBackend, NetWake};
use rustdroid_net::NatConfig;
use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{FIXED_INFO_W2KSP1, GetNetworkParams};

/// DNS-серверы хоста строками (`GetNetworkParams`); пусто, если узнать не удалось.
fn system_dns_servers() -> Vec<String> {
    let mut len = 0u32;
    // SAFETY: первый вызов без буфера только сообщает нужный размер в `len`.
    let first = unsafe { GetNetworkParams(None, &raw mut len) };
    if first != ERROR_BUFFER_OVERFLOW || len == 0 {
        return Vec::new();
    }
    // Буфер выровнен по 8 байт: внутри структура с указателями.
    let words = usize::try_from(len).unwrap_or(0).div_ceil(8);
    let mut buf = vec![0u64; words];
    let info = buf.as_mut_ptr().cast::<FIXED_INFO_W2KSP1>();
    // SAFETY: буфер не короче `len` байт и выровнен для FIXED_INFO_W2KSP1; Windows заполняет
    // структуру и цепочку адресов внутри этого же буфера.
    let r = unsafe { GetNetworkParams(Some(info), &raw mut len) };
    if r != ERROR_SUCCESS {
        return Vec::new();
    }
    let mut out = Vec::new();
    // SAFETY: вызов успешен, `info` указывает на заполненную структуру в `buf`.
    let mut node = unsafe { &raw const (*info).DnsServerList };
    while !node.is_null() && out.len() < 8 {
        // SAFETY: `node` — элемент цепочки внутри `buf` (первый — поле структуры, остальные —
        // `Next`, которые Windows расположила в том же буфере).
        let entry = unsafe { &*node };
        let bytes: Vec<u8> = entry
            .IpAddress
            .String
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| c.cast_unsigned())
            .collect();
        out.push(String::from_utf8_lossy(&bytes).into_owned());
        node = entry.Next;
    }
    out
}

/// Бэкенд `--net user`: NAT с DNS хоста и будильник очереди приёма для ВМ.
pub(crate) fn user_net() -> Result<(Box<dyn NetBackend>, Arc<NetWake>)> {
    let servers = system_dns_servers();
    let dns = rustdroid_net::pick_dns(servers.iter().map(String::as_str));
    tracing::info!(?servers, dns = ?dns, "DNS хоста для сети гостя");
    let mut cfg = NatConfig::default();
    if let Some(ip) = dns {
        cfg.upstream_dns = SocketAddr::from((ip, 53));
    }
    let wake = NetWake::new();
    let backend =
        NatBackend::new(cfg, Arc::clone(&wake)).context("не удалось запустить NAT гостя")?;
    Ok((Box::new(backend), wake))
}
