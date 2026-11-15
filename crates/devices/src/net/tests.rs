use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustdroid_net::{GATEWAY_IP, GATEWAY_MAC, GUEST_IP};
use rustdroid_virtio::mem::{GuestMemoryExt, VecMemory};
use rustdroid_virtio::queue::{VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};

use super::*;

fn queue(base: u64) -> Queue {
    let mut q = Queue::new(8);
    q.desc_table = base;
    q.avail_ring = base + 0x1000;
    q.used_ring = base + 0x2000;
    q.ready = true;
    q
}

/// Кладёт в очередь цепочку дескрипторов `(адрес, длина, флаги записи)` с головой в `first`.
fn post(mem: &VecMemory, q: &Queue, first: u16, avail_slot: u16, parts: &[(u64, u32, bool)]) {
    for (i, (addr, len, write)) in parts.iter().enumerate() {
        let slot = first + u16::try_from(i).unwrap();
        let mut flags = if *write { VIRTQ_DESC_F_WRITE } else { 0 };
        let next = if i + 1 < parts.len() {
            flags |= VIRTQ_DESC_F_NEXT;
            slot + 1
        } else {
            0
        };
        let mut b = [0_u8; 16];
        b[..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        mem.write(q.desc_table + u64::from(slot) * 16, &b).unwrap();
    }
    mem.write_u16(q.avail_ring + 4 + u64::from(avail_slot) * 2, first)
        .unwrap();
    mem.write_u16(q.avail_ring + 2, avail_slot + 1).unwrap();
}

#[derive(Default)]
struct Fake {
    sent: Vec<Vec<u8>>,
    to_guest: VecDeque<Vec<u8>>,
}

struct FakeBackend(Arc<Mutex<Fake>>);

impl NetBackend for FakeBackend {
    fn send(&mut self, frame: &[u8]) {
        self.0.lock().unwrap().sent.push(frame.to_vec());
    }
    fn recv(&mut self) -> Option<Vec<u8>> {
        self.0.lock().unwrap().to_guest.pop_front()
    }
    fn unrecv(&mut self, frame: Vec<u8>) {
        self.0.lock().unwrap().to_guest.push_front(frame);
    }
}

fn device() -> (VirtioNet, Arc<Mutex<Fake>>) {
    let fake = Arc::new(Mutex::new(Fake::default()));
    (
        VirtioNet::new(Box::new(FakeBackend(Arc::clone(&fake))), DEFAULT_MAC),
        fake,
    )
}

fn used_len(mem: &VecMemory, q: &Queue, slot: u64) -> u32 {
    let mut b = [0u8; 4];
    mem.read(q.used_ring + 4 + slot * 8 + 4, &mut b).unwrap();
    u32::from_le_bytes(b)
}

#[test]
fn config_has_mac_and_link_up() {
    let (dev, _) = device();
    assert_eq!(dev.device_type(), TYPE_NET);
    assert_eq!(dev.device_features(), F_MAC | F_STATUS);
    let mut cfg = [0u8; 8];
    dev.read_config(0, &mut cfg);
    assert_eq!(&cfg[..6], &DEFAULT_MAC);
    assert_eq!(&cfg[6..], &[1, 0]);
    let mut tail = [0xffu8; 2];
    dev.read_config(7, &mut tail);
    assert_eq!(tail, [0, 0]);
}

#[test]
fn guest_frame_reaches_backend_without_header() {
    let mem = VecMemory::new(0x20000);
    let (mut dev, fake) = device();
    let mut tx = queue(0x1000);
    // Заголовок и кадр — в разных дескрипторах.
    mem.write(0x10000, &[0u8; HDR_LEN]).unwrap();
    mem.write(0x10100, b"ethernet-frame").unwrap();
    post(
        &mem,
        &tx,
        0,
        0,
        &[(0x10000, 12, false), (0x10100, 14, false)],
    );
    assert!(dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap());
    assert_eq!(fake.lock().unwrap().sent, vec![b"ethernet-frame".to_vec()]);
    assert_eq!(used_len(&mem, &tx, 0), 0);
}

#[test]
fn backend_frame_is_delivered_with_header() {
    let mem = VecMemory::new(0x20000);
    let (mut dev, fake) = device();
    let mut rx = queue(0x8000);
    fake.lock()
        .unwrap()
        .to_guest
        .push_back(b"incoming".to_vec());
    post(&mem, &rx, 0, 0, &[(0x11000, 2048, true)]);
    assert!(dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
    assert_eq!(used_len(&mem, &rx, 0), 12 + 8);
    let mut b = [0u8; 20];
    mem.read(0x11000, &mut b).unwrap();
    // num_buffers = 1, остальные поля заголовка — нули.
    assert_eq!(&b[..12], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(&b[12..], b"incoming");
}

#[test]
fn frame_is_split_across_guest_buffers() {
    let mem = VecMemory::new(0x20000);
    let (mut dev, fake) = device();
    let mut rx = queue(0x8000);
    fake.lock()
        .unwrap()
        .to_guest
        .push_back(b"0123456789".to_vec());
    // Первый буфер вмещает заголовок и 3 байта кадра, второй — остальное.
    post(&mem, &rx, 0, 0, &[(0x11000, 15, true), (0x12000, 64, true)]);
    assert!(dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
    assert_eq!(used_len(&mem, &rx, 0), 22);
    let mut a = [0u8; 15];
    mem.read(0x11000, &mut a).unwrap();
    assert_eq!(&a[12..], b"012");
    let mut b = [0u8; 7];
    mem.read(0x12000, &mut b).unwrap();
    assert_eq!(&b, b"3456789");
}

#[test]
fn frame_waits_until_guest_provides_buffers() {
    let mem = VecMemory::new(0x20000);
    let (mut dev, fake) = device();
    let mut rx = queue(0x8000);
    {
        let mut f = fake.lock().unwrap();
        f.to_guest.push_back(b"first".to_vec());
        f.to_guest.push_back(b"second".to_vec());
    }
    // Буферов нет: ничего не доставлено, кадры не потеряны и порядок сохранён.
    assert!(!dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
    assert_eq!(fake.lock().unwrap().to_guest.len(), 2);
    post(&mem, &rx, 0, 0, &[(0x11000, 2048, true)]);
    assert!(dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
    // Один буфер — один кадр, второй ждёт.
    assert_eq!(fake.lock().unwrap().to_guest.len(), 1);
    let mut b = [0u8; 5];
    mem.read(0x11000 + 12, &mut b).unwrap();
    assert_eq!(&b, b"first");
    assert_eq!(fake.lock().unwrap().to_guest.front().unwrap(), b"second");
}

#[test]
fn nat_backend_answers_dhcp_and_wakes_receive_queue() {
    let mem = VecMemory::new(0x20000);
    let wake = NetWake::new();
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = Arc::clone(&wakes);
    wake.set_waker(move || {
        w.fetch_add(1, Ordering::SeqCst);
    });
    let backend = NatBackend::new(NatConfig::default(), Arc::clone(&wake)).unwrap();
    let mut dev = VirtioNet::new(Box::new(backend), DEFAULT_MAC);
    let (mut tx, mut rx) = (queue(0x1000), queue(0x8000));

    // DHCP DISCOVER гостя в кадре Ethernet.
    let mut dhcp = vec![0u8; 236];
    dhcp[0] = 1;
    dhcp[1] = 1;
    dhcp[2] = 6;
    dhcp[4..8].copy_from_slice(&0x1234u32.to_be_bytes());
    dhcp[28..34].copy_from_slice(&DEFAULT_MAC);
    dhcp.extend_from_slice(&[0x63, 0x82, 0x53, 0x63, 53, 1, 1, 255]);
    let udp =
        rustdroid_net::wire::build_udp(Ipv4Addr::UNSPECIFIED, Ipv4Addr::BROADCAST, 68, 67, &dhcp);
    let ip = rustdroid_net::wire::build_ipv4(
        Ipv4Addr::UNSPECIFIED,
        Ipv4Addr::BROADCAST,
        rustdroid_net::wire::PROTO_UDP,
        &udp,
    );
    let frame = rustdroid_net::wire::build_eth(
        rustdroid_net::wire::BROADCAST,
        DEFAULT_MAC,
        rustdroid_net::wire::ETHERTYPE_IPV4,
        &ip,
    );
    let mut pkt = vec![0u8; HDR_LEN];
    pkt.extend_from_slice(&frame);
    mem.write(0x10000, &pkt).unwrap();
    post(
        &mem,
        &tx,
        0,
        0,
        &[(0x10000, u32::try_from(pkt.len()).unwrap(), false)],
    );
    assert!(dev.process_queue(TX_QUEUE, &mut tx, &mem).unwrap());
    assert!(
        wakes.load(Ordering::SeqCst) >= 1,
        "очередь приёма разбужена"
    );

    post(&mem, &rx, 0, 0, &[(0x11000, 2048, true)]);
    assert!(dev.process_queue(RX_QUEUE, &mut rx, &mem).unwrap());
    let len = usize::try_from(used_len(&mem, &rx, 0)).unwrap();
    let mut got = vec![0u8; len];
    mem.read(0x11000, &mut got).unwrap();
    let eth = rustdroid_net::wire::parse_eth(&got[HDR_LEN..]).unwrap();
    assert_eq!((eth.dst, eth.src), (DEFAULT_MAC, GATEWAY_MAC));
    let ip = rustdroid_net::wire::parse_ipv4(eth.payload).unwrap();
    let u = rustdroid_net::wire::parse_udp(ip.payload).unwrap();
    assert_eq!(ip.src, GATEWAY_IP);
    assert_eq!(&u.payload[16..20], &GUEST_IP.octets());
}
