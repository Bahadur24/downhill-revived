//! The offline LAN room service. The host runs this small WebSocket room coordinator in-process;
//! its normal `NetClient` connects back to loopback and guest clients connect to the host's private
//! IPv4 address. Lobby messages reuse protocol 4 and race STATE/EVENT frames are relayed by the host.
//! This is deliberately independent of the online service, SFU and P2P signaling.

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};

use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tungstenite::{Bytes, Message, Utf8Bytes};

const PROTOCOL: u64 = 4;
const MAX_PLAYERS: usize = 10;
const MAX_FRAME: usize = 3072;
const OUTBOUND_QUEUE: usize = 512;
const READ_TIMEOUT: Duration = Duration::from_millis(40);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const START_LEAD_MS: f64 = 3000.0;
const RACE_GO_MAX_MS: f64 = 600_000.0;
const DISCOVERY_PORT: u16 = 48123;
const DISCOVERY_MAGIC: &[u8] = b"DHDL";
const DISCOVERY_VERSION: u8 = 2;
const DISCOVERY_PROTOCOL: u8 = 4;
const DISCOVERY_PACKET_MAX: usize = 128;
const PIN_ATTEMPT_WINDOW: Duration = Duration::from_secs(60);
const PIN_THROTTLE: Duration = Duration::from_secs(30);
const PIN_IP_LIMIT: usize = 256;
const DISCOVERY_SCAN_MS: u64 = 1500;
const RECONNECT_GRACE: Duration = Duration::from_secs(60);
const CHECK_TIMEOUT_MS: f64 = 5000.0;
const CHECK_FRESH_MS: f64 = 15_000.0;

#[derive(Clone)]
enum Outbound {
    Text(String),
    Binary(Vec<u8>),
    Close(u16, String),
}

struct Member {
    id: u64,
    name: String,
    ready: bool,
    choosing: bool,
    rider: u64,
    bike: u64,
    rtt_ms: Option<u64>,
    tx: SyncSender<Outbound>,
    stopped: Arc<AtomicBool>,
    slot: Option<u8>,
    race_state: String,
    dnf_reason: String,
    finish_ticks: Option<u64>,
    place: u64,
    token_hash: [u8; 32],
    generation: u64,
    disconnected_until: Option<Instant>,
    last_check_id: u64,
    last_check_ok: bool,
}

struct Room {
    members: Vec<Member>,
    host_id: Option<u64>,
    next_id: u64,
    version: String,
    iso: String,
    room_name: String,
    track: u64,
    laps: u64,
    seed: Option<u64>,
    max_players: usize,
    finish_window: u64,
    next_race: String,
    then: String,
    rotation: Vec<u64>,
    track_limits: Vec<u64>,
    pick_only: Vec<u64>,
    started: bool,
    race_id: u64,
    last_race_id: u64,
    last_race_track: u64,
    last_race_entrants: Vec<Value>,
    start_at: Option<f64>,
    race_go_at: Option<f64>,
    finish_ends_at: Option<f64>,
    closed: bool,
    check_id: u64,
    check_state: String,
    check_deadline: Option<f64>,
    check_started_at: Option<f64>,
    room_id: [u8; 16],
    pin_salt: Option<[u8; 16]>,
    pin_verifier: Option<[u8; 32]>,
    pin_attempts: HashMap<String, Vec<Instant>>,
}

impl Default for Room {
    fn default() -> Self {
        Self {
            members: Vec::new(),
            host_id: None,
            next_id: 0,
            version: String::new(),
            iso: String::new(),
            room_name: String::new(),
            track: 0,
            laps: 0,
            seed: None,
            max_players: MAX_PLAYERS,
            finish_window: 60,
            next_race: "host".into(),
            then: "same".into(),
            rotation: Vec::new(),
            track_limits: Vec::new(),
            pick_only: Vec::new(),
            started: false,
            race_id: 0,
            last_race_id: 0,
            last_race_track: 0,
            last_race_entrants: Vec::new(),
            start_at: None,
            race_go_at: None,
            finish_ends_at: None,
            closed: false,
            check_id: 0,
            check_state: "idle".into(),
            check_deadline: None,
            check_started_at: None,
            room_id: [0; 16],
            pin_salt: None,
            pin_verifier: None,
            pin_attempts: HashMap::new(),
        }
    }
}

struct Server {
    stop: Arc<AtomicBool>,
    room: Arc<Mutex<Room>>,
    address: String,
    tailnet: bool,
    advertised: Arc<Mutex<String>>,
    thread: Option<JoinHandle<()>>,
    discovery_thread: Option<JoinHandle<()>>,
}

static SERVER: OnceLock<Mutex<Option<Server>>> = OnceLock::new();

fn server_slot() -> &'static Mutex<Option<Server>> {
    SERVER.get_or_init(|| Mutex::new(None))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn epoch_ms() -> f64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0)
        .floor()
}

fn route_ipv4() -> Option<Ipv4Addr> {
    // A UDP connect selects a source address without sending a packet. TEST-NET-3 is used only for
    // route selection; on Unix we accept it only when it matches a real, non-virtual interface.
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("203.0.113.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

#[cfg(unix)]
fn virtual_interface(name: &str) -> bool {
    [
        "tun",
        "tap",
        "wg",
        "utun",
        "ppp",
        "rmnet",
        "ccmni",
        "veth",
        "dummy",
        "br-",
        "virbr",
        "tailscale",
        "zt",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

#[cfg(unix)]
fn wifi_interface(name: &str) -> bool {
    if name.starts_with("wlan")
        || name.starts_with("swlan")
        || name.starts_with("wl")
        || name.starts_with("ap")
    {
        return true;
    }
    #[cfg(target_os = "macos")]
    if name == "en0" {
        return true;
    }
    false
}

#[cfg(unix)]
fn select_local_ipv4(
    mut candidates: Vec<(String, Ipv4Addr)>,
    route: Option<Ipv4Addr>,
) -> Result<Ipv4Addr, String> {
    candidates.retain(|(name, ip)| {
        ip.is_private() && !ip.is_loopback() && !ip.is_unspecified() && !virtual_interface(name)
    });
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let wifi: Vec<Ipv4Addr> = candidates
        .iter()
        .filter(|(name, _)| wifi_interface(name))
        .map(|(_, ip)| *ip)
        .collect();
    if wifi.len() == 1 {
        return Ok(wifi[0]);
    }
    if wifi.len() > 1 {
        if let Some(ip) = route.filter(|route_ip| wifi.contains(route_ip)) {
            return Ok(ip);
        }
        return Err(
            "Several Wi-Fi addresses are active; disconnect extra networks and try hosting again."
                .into(),
        );
    }
    if let Some(ip) = route.filter(|route_ip| candidates.iter().any(|(_, ip)| ip == route_ip)) {
        return Ok(ip);
    }
    match candidates.as_slice() {
        [(_, ip)] => Ok(*ip),
        [] => Err("Connect this device to a private Wi-Fi network, then try hosting again.".into()),
        _ => Err(
            "Can't identify the LAN interface. Turn off VPNs or leave one local network active."
                .into(),
        ),
    }
}

#[cfg(unix)]
fn local_ipv4() -> Result<Ipv4Addr, String> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err("Can't inspect local network interfaces.".into());
    }
    let mut candidates = Vec::new();
    let mut current = head;
    while !current.is_null() {
        let interface = unsafe { &*current };
        if !interface.ifa_name.is_null()
            && !interface.ifa_addr.is_null()
            && interface.ifa_flags & (libc::IFF_UP as u32) != 0
            && interface.ifa_flags & (libc::IFF_LOOPBACK as u32) == 0
            && unsafe { (*interface.ifa_addr).sa_family as i32 } == libc::AF_INET
        {
            let address = unsafe { &*(interface.ifa_addr.cast::<libc::sockaddr_in>()) };
            let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
            let name = unsafe { CStr::from_ptr(interface.ifa_name) }
                .to_string_lossy()
                .into_owned();
            candidates.push((name, ip));
        }
        current = interface.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    select_local_ipv4(candidates, route_ipv4())
}

#[cfg(not(unix))]
fn local_ipv4() -> Result<Ipv4Addr, String> {
    let ip = route_ipv4()
        .ok_or_else(|| "Connect this device to Wi-Fi, then try hosting again.".to_string())?;
    if ip.is_private() && !ip.is_loopback() && !ip.is_unspecified() {
        Ok(ip)
    } else {
        Err("Connect this device to a private local network, then try hosting again.".into())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LanInterface {
    pub name: [c_char; 16],
    pub address: [c_char; 16],
    pub is_wifi: i32,
    pub selected: i32,
}

#[cfg(unix)]
fn local_interfaces() -> Vec<(String, Ipv4Addr, bool)> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        let x = unsafe { &*cur };
        if !x.ifa_name.is_null()
            && !x.ifa_addr.is_null()
            && x.ifa_flags & (libc::IFF_UP as u32) != 0
            && x.ifa_flags & (libc::IFF_LOOPBACK as u32) == 0
            && unsafe { (*x.ifa_addr).sa_family as i32 } == libc::AF_INET
        {
            let sin = unsafe { &*(x.ifa_addr.cast::<libc::sockaddr_in>()) };
            let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            let name = unsafe { CStr::from_ptr(x.ifa_name) }
                .to_string_lossy()
                .into_owned();
            if ip.is_private()
                && !ip.is_loopback()
                && !ip.is_unspecified()
                && !virtual_interface(&name)
            {
                out.push((name.clone(), ip, wifi_interface(&name)));
            }
        }
        cur = x.ifa_next;
    }
    unsafe {
        libc::freeifaddrs(head);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    out
}
#[cfg(not(unix))]
fn local_interfaces() -> Vec<(String, Ipv4Addr, bool)> {
    local_ipv4()
        .ok()
        .map(|ip| vec![("local".into(), ip, false)])
        .unwrap_or_default()
}

fn live_host_address(
    current: &str,
    port: u16,
    interfaces: &[(String, Ipv4Addr, bool)],
    fallback: Option<Ipv4Addr>,
) -> Option<String> {
    let assigned = |ip: &Ipv4Addr| interfaces.iter().any(|(_, address, _)| address == ip);
    let current_ip = current
        .split(':')
        .next()
        .and_then(|text| text.parse::<Ipv4Addr>().ok());
    current_ip
        .filter(assigned)
        .or_else(|| fallback.filter(assigned))
        .map(|ip| format!("{ip}:{port}"))
}

fn pin_valid(pin: &str) -> bool {
    (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}
fn pin_verifier(salt: &[u8; 16], pin: &str) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(16 + pin.len());
    bytes.extend_from_slice(salt);
    bytes.extend_from_slice(pin.as_bytes());
    token_hash(&bytes)
}
fn pin_admission(
    state: &mut Room,
    peer_ip: &str,
    pin: &str,
    now: Instant,
) -> Result<(), &'static str> {
    let Some(verifier) = state.pin_verifier else {
        return Ok(());
    };
    let attempts = state.pin_attempts.entry(peer_ip.to_string()).or_default();
    if attempts.len() >= 5
        && attempts
            .last()
            .is_some_and(|t| now.duration_since(*t) < PIN_ATTEMPT_WINDOW + PIN_THROTTLE)
    {
        return Err("pin_throttled");
    }
    attempts.retain(|t| now.duration_since(*t) < PIN_ATTEMPT_WINDOW);
    let valid = pin_valid(pin)
        && state
            .pin_salt
            .is_some_and(|salt| pin_verifier(&salt, pin).ct_eq(&verifier).into());
    if valid {
        return Ok(());
    }
    attempts.push(now);
    if state.pin_attempts.len() > PIN_IP_LIMIT {
        if let Some(k) = state.pin_attempts.keys().next().cloned() {
            state.pin_attempts.remove(&k);
        }
    }
    Err(if pin.is_empty() {
        "pin_required"
    } else {
        "pin_invalid"
    })
}
fn new_room_id() -> Result<[u8; 16], String> {
    let mut id = [0; 16];
    SystemRandom::new()
        .fill(&mut id)
        .map_err(|_| "secure random unavailable".into())
        .map(|_| id)
}
fn room_id_string(id: &[u8; 16]) -> String {
    id.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn token_hash(token: &[u8]) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, token);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}
fn token_matches(hash: &[u8; 32], token: &[u8]) -> bool {
    bool::from(hash.ct_eq(&token_hash(token)))
}
fn new_token() -> Result<([u8; 32], String), String> {
    let mut token = [0u8; 32];
    SystemRandom::new()
        .fill(&mut token)
        .map_err(|_| "secure random unavailable".to_string())?;
    let token: String = token.iter().map(|b| format!("{b:02x}")).collect();
    Ok((token_hash(token.as_bytes()), token))
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LanDiscoverResult {
    pub address: [c_char; 64],
    pub room_name: [c_char; 32],
    pub players: i32,
    pub track: i32,
    pub room_id: [c_char; 17],
    pub pin_required: i32,
    pub max_players: i32,
    pub rtt_ms: i32,
}

fn discovery_packet(nonce: &[u8; 16]) -> Vec<u8> {
    let mut p = Vec::with_capacity(24);
    p.extend_from_slice(DISCOVERY_MAGIC);
    p.push(DISCOVERY_VERSION);
    p.push(DISCOVERY_PROTOCOL);
    p.extend_from_slice(nonce);
    p
}
fn valid_discovery_request(
    data: &[u8],
    source: SocketAddrV4,
    local_port: u16,
    nonce: &[u8; 16],
) -> bool {
    data == discovery_packet(nonce).as_slice()
        && source.ip().is_private()
        && !source.ip().is_loopback()
        && !source.ip().is_unspecified()
        && source.port() != 0
        && local_port == DISCOVERY_PORT
}
fn discovery_response(
    address: &str,
    room_name: &str,
    players: usize,
    max_players: usize,
    track: u64,
    nonce: &[u8; 16],
    room_id: &[u8; 16],
    pin_required: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(DISCOVERY_PACKET_MAX);
    out.extend_from_slice(b"DHDL");
    out.push(DISCOVERY_VERSION);
    out.push(DISCOVERY_PROTOCOL);
    out.extend_from_slice(nonce);
    out.push(players.min(255) as u8);
    out.push(track.min(255) as u8);
    out.push(max_players.min(255) as u8);
    for text in [address, room_name] {
        let bytes = text.as_bytes();
        out.push(bytes.len().min(31) as u8);
        out.extend_from_slice(&bytes[..bytes.len().min(31)]);
    }
    let id = room_id_string(room_id);
    out.push(id.len() as u8);
    out.extend_from_slice(id.as_bytes());
    out.push(pin_required as u8);
    out
}

#[derive(Default)]
struct DiscoverState {
    working: bool,
    failed: bool,
    results: Vec<LanDiscoverResult>,
    cancel: Option<Arc<AtomicBool>>,
}
static DISCOVER: OnceLock<Mutex<Option<DiscoverState>>> = OnceLock::new();
fn discover_slot() -> &'static Mutex<Option<DiscoverState>> {
    DISCOVER.get_or_init(|| Mutex::new(None))
}
fn cancel_discovery_scan() {
    let cancel = lock(discover_slot())
        .as_ref()
        .and_then(|state| state.cancel.clone());
    if let Some(cancel) = cancel {
        cancel.store(true, Ordering::Release);
    }
}
fn c_array<const N: usize>(s: &str) -> [c_char; N] {
    let mut a = [0 as c_char; N];
    for (i, b) in s.bytes().take(N - 1).enumerate() {
        a[i] = b as c_char;
    }
    a
}
fn parse_discovery_response(data: &[u8], nonce: &[u8; 16]) -> Option<LanDiscoverResult> {
    if data.len() < 25
        || &data[..4] != b"DHDL"
        || data[4] != DISCOVERY_VERSION
        || data[5] != DISCOVERY_PROTOCOL
        || &data[6..22] != nonce
    {
        return None;
    }
    let max_players = data[24] as i32;
    if !(2..=MAX_PLAYERS as i32).contains(&max_players) || data[22] as i32 >= max_players {
        return None;
    }
    let mut i = 25;
    let take = |i: &mut usize| {
        if *i >= data.len() {
            return None;
        };
        let n = data[*i] as usize;
        *i += 1;
        if n > 31 || *i + n > data.len() {
            return None;
        };
        let s = String::from_utf8_lossy(&data[*i..*i + n]).into_owned();
        *i += n;
        Some(s)
    };
    let address = take(&mut i)?;
    let room = take(&mut i)?;
    if address.parse::<SocketAddrV4>().ok().is_none() {
        return None;
    }
    let (id, required) = if i < data.len() {
        let n = data[i] as usize;
        i += 1;
        if n != 16 || i + n + 1 > data.len() {
            return None;
        }
        let id = String::from_utf8_lossy(&data[i..i + n]).into_owned();
        i += n;
        (id, data[i] != 0)
    } else {
        (String::new(), false)
    };
    Some(LanDiscoverResult {
        address: c_array(&address),
        room_name: c_array(&room),
        players: data[22] as i32,
        track: data[23] as i32,
        room_id: c_array(&id),
        pin_required: required as i32,
        max_players,
        rtt_ms: -1,
    })
}
fn discovery_room_open(state: &Room) -> bool {
    !state.closed
        && !state.started
        && state.host_id.is_some()
        && !state.members.is_empty()
        && state.members.len() < state.max_players
}

fn sort_discovery_results(results: &mut [LanDiscoverResult]) {
    results.sort_by(|a, b| {
        let free_a = a.max_players.saturating_sub(a.players);
        let free_b = b.max_players.saturating_sub(b.players);
        a.rtt_ms
            .cmp(&b.rtt_ms)
            .then_with(|| free_b.cmp(&free_a))
            .then_with(|| a.address.cmp(&b.address))
    });
}

fn start_discovery_scan() -> bool {
    let mut slot = lock(discover_slot());
    if slot.as_ref().is_some_and(|s| s.working) {
        return true;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    *slot = Some(DiscoverState {
        working: true,
        cancel: Some(cancel.clone()),
        ..Default::default()
    });
    drop(slot);
    let spawned = thread::Builder::new()
        .name("dhd-lan-scan".into())
        .spawn(move || {
            let result = (|| {
                let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
                socket.set_broadcast(true).ok()?;
                socket
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .ok()?;
                let mut nonce = [0u8; 16];
                SystemRandom::new().fill(&mut nonce).ok()?;
                let packet = discovery_packet(&nonce);
                let scan_started = Instant::now();
                let _ = socket.send_to(
                    &packet,
                    SocketAddrV4::new(Ipv4Addr::BROADCAST, DISCOVERY_PORT),
                );
                let until = Instant::now() + Duration::from_millis(DISCOVERY_SCAN_MS);
                let mut seen = HashMap::new();
                let mut buf = [0u8; DISCOVERY_PACKET_MAX];
                while Instant::now() < until && !cancel.load(Ordering::Acquire) {
                    if let Ok((n, src)) = socket.recv_from(&mut buf) {
                        if let std::net::SocketAddr::V4(v4) = src {
                            if let Some(mut r) = parse_discovery_response(&buf[..n], &nonce) {
                                let advertised =
                                    unsafe { CStr::from_ptr(r.address.as_ptr()) }.to_string_lossy();
                                if let Ok(address) = advertised.parse::<SocketAddrV4>() {
                                    if valid_guest_address(address)
                                        && address.ip() == v4.ip()
                                        && v4.ip().is_private()
                                        && !v4.ip().is_loopback()
                                    {
                                        r.rtt_ms = scan_started
                                            .elapsed()
                                            .as_millis()
                                            .min(i32::MAX as u128)
                                            as i32;
                                        seen.entry(r.address).or_insert(r);
                                    }
                                }
                            }
                        }
                    }
                }
                let mut results = seen.into_values().collect::<Vec<_>>();
                sort_discovery_results(&mut results);
                Some(results.into_iter().take(8).collect::<Vec<_>>())
            })();
            let mut slot = lock(discover_slot());
            if let Some(s) = slot.as_mut().filter(|s| {
                s.cancel
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &cancel))
            }) {
                s.working = false;
                s.cancel = None;
                match result {
                    Some(v) => s.results = v,
                    None => s.failed = true,
                }
            }
        });
    if spawned.is_err() {
        *lock(discover_slot()) = None;
        return false;
    }
    true
}

fn rotate_track(state: &Room, current: u64, seed: u64) -> u64 {
    match state.then.as_str() {
        "same" => current,
        "next" => {
            let eligible: Vec<_> = state
                .rotation
                .iter()
                .copied()
                .filter(|t| !state.pick_only.contains(t))
                .collect();
            if eligible.is_empty() {
                current
            } else {
                match eligible.iter().position(|t| *t == current) {
                    Some(i) => eligible[(i + 1) % eligible.len()],
                    None => eligible[0],
                }
            }
        }
        "random" => {
            let eligible: Vec<_> = state
                .rotation
                .iter()
                .copied()
                .filter(|t| !state.pick_only.contains(t))
                .collect();
            if eligible.is_empty() {
                current
            } else {
                eligible[(seed as usize) % eligible.len()]
            }
        }
        _ => current,
    }
}

fn check_valid(state: &Room, now: f64) -> bool {
    state.check_id != 0
        && state.check_state == "passed"
        && state
            .check_started_at
            .is_some_and(|at| now >= at && now - at <= CHECK_FRESH_MS)
        && state.members.iter().filter(|m| m.ready).all(|m| {
            !m.stopped.load(Ordering::Acquire)
                && m.last_check_id == state.check_id
                && m.last_check_ok
        })
}

fn tailnet_peer_allowed(peer: std::net::SocketAddr) -> bool {
    match peer.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback() || is_tailnet_ipv4(ip),
        std::net::IpAddr::V6(ip) => ip.is_loopback(),
    }
}

fn start_server(ip: Ipv4Addr, tailnet: bool) -> Result<Server, String> {
    let room_id = new_room_id()?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .map_err(|e| format!("can't open a LAN port ({e})"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("can't prepare LAN listener ({e})"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("can't read LAN port ({e})"))?
        .port();
    let address = format!("{ip}:{port}");
    let stop = Arc::new(AtomicBool::new(false));
    let room = Arc::new(Mutex::new(Room {
        room_id,
        ..Room::default()
    }));
    let discovery_stop = stop.clone();
    let advertised = Arc::new(Mutex::new(address.clone()));
    let discovery_address = advertised.clone();
    let discovery_room = room.clone();
    let discovery_room_thread = discovery_room.clone();
    let discovery_thread = if tailnet {
        None
    } else {
        thread::Builder::new()
            .name("dhd-lan-discovery".into())
            .spawn(move || {
                let Ok(sock) =
                    UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))
                else {
                    return;
                };
                let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
                let mut buf = [0u8; DISCOVERY_PACKET_MAX];
                while !discovery_stop.load(Ordering::Acquire) {
                    if let Ok((n, src)) = sock.recv_from(&mut buf) {
                        if let std::net::SocketAddr::V4(v4) = src {
                            let mut nonce = [0u8; 16];
                            if n >= 22 {
                                nonce.copy_from_slice(&buf[6..22]);
                            }
                            if valid_discovery_request(&buf[..n], v4, DISCOVERY_PORT, &nonce) {
                                let current = lock(&discovery_address).clone();
                                let address = live_host_address(
                                    &current,
                                    port,
                                    &local_interfaces(),
                                    local_ipv4().ok(),
                                );
                                let Some(address) = address else {
                                    lock(&discovery_address).clear();
                                    continue;
                                };
                                *lock(&discovery_address) = address.clone();
                                let st = lock(&discovery_room_thread);
                                if !discovery_room_open(&st) {
                                    continue;
                                }
                                let _ = sock.send_to(
                                    &discovery_response(
                                        &address,
                                        &st.room_name,
                                        st.members.len(),
                                        st.max_players,
                                        st.track,
                                        &nonce,
                                        &st.room_id,
                                        st.pin_verifier.is_some(),
                                    ),
                                    src,
                                );
                            }
                        }
                    }
                }
            })
            .ok()
    };
    let thread_stop = stop.clone();
    let thread_room = room.clone();
    let thread = thread::Builder::new()
        .name("dhd-lan-room".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                finish_window_tick(&thread_room);
                check_timeout_tick(&thread_room);
                expire_disconnected(&thread_room, &thread_stop);
                if thread_stop.load(Ordering::Acquire) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, peer)) => {
                        if tailnet && !tailnet_peer_allowed(peer) {
                            eprintln!("net: Tailscale room rejected non-tailnet peer {peer}");
                            continue;
                        }
                        let r = thread_room.clone();
                        let s = thread_stop.clone();
                        if let Err(e) = thread::Builder::new()
                            .name("dhd-lan-peer".into())
                            .spawn(move || peer_main(stream, peer.to_string(), r, s))
                        {
                            eprintln!("net: LAN couldn't start a peer thread ({e})");
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => {
                        eprintln!("net: LAN listener failed ({e})");
                        thread_stop.store(true, Ordering::Release);
                        close_room(&thread_room, "LAN host stopped");
                        break;
                    }
                }
            }
        })
        .map_err(|e| format!("can't start LAN room ({e})"))?;
    Ok(Server {
        stop,
        room,
        address,
        tailnet,
        advertised,
        thread: Some(thread),
        discovery_thread,
    })
}

fn close_room(room: &Arc<Mutex<Room>>, reason: &str) {
    let mut state = lock(room);
    state.closed = true;
    for member in &state.members {
        queue(
            member,
            Outbound::Text(
                json!({"type":"error","code":"room_not_found","message":reason}).to_string(),
            ),
        );
        member.stopped.store(true, Ordering::Release);
    }
}

fn queue(member: &Member, out: Outbound) {
    match member.tx.try_send(out) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            // Never silently lose an EVENT or an unbounded backlog. Terminate the slow consumer;
            // its normal socket-close path tells the other games that its rider left.
            eprintln!("net: LAN peer {} exceeded its outbound queue", member.id);
            member.stopped.store(true, Ordering::Release);
        }
        Err(TrySendError::Disconnected(_)) => member.stopped.store(true, Ordering::Release),
    }
}

fn broadcast(state: &Room, out: Outbound, except: Option<u64>) {
    for m in &state.members {
        if Some(m.id) != except && !m.stopped.load(Ordering::Acquire) {
            queue(m, out.clone());
        }
    }
}

fn send_json(tx: &SyncSender<Outbound>, value: Value) {
    let _ = tx.try_send(Outbound::Text(value.to_string()));
}

fn room_message(state: &Room) -> Value {
    let members: Vec<Value> = state
        .members
        .iter()
        .map(|m| {
            let in_race = state.started && m.slot.is_some();
            let mut item = json!({
                "id": m.id, "name": m.name, "ready": m.ready, "rider": m.rider, "bike": m.bike,
                "rttMs": m.rtt_ms, "choosing": m.choosing, "inRace": in_race,
                "checkId":if state.check_id != 0 {state.check_id} else {m.last_check_id},
                "checkState":if m.last_check_id == state.check_id && m.last_check_ok {"ok"}
                    else if state.check_state == "pending" {"pending"}
                    else if state.check_state == "timeout" || state.check_state == "failed" {"timeout"} else {"none"},
            });
            if in_race {
                item["raceState"] = json!(m.race_state);
                if !m.dnf_reason.is_empty() {
                    item["dnfReason"] = json!(m.dnf_reason);
                }
                item["place"] = json!(m.place);
                if let Some(ticks) = m.finish_ticks {
                    item["finishTicks"] = json!(ticks);
                }
            }
            item
        })
        .collect();
    json!({
        "type":"room", "code":"LAN", "hostId":state.host_id.unwrap_or(0), "roomName":state.room_name,
        "roomId":room_id_string(&state.room_id), "pinRequired":state.pin_verifier.is_some(),
        "public":false, "maxPlayers":state.max_players, "started":state.started,
        "version":state.version, "phase":if state.started {"racing"} else {"open"},
        "settings":{"track":state.track,"laps":state.laps,"seed":state.seed},
        "members":members, "countdownEndsAt":null, "finishEndsAt":state.finish_ends_at,
        "serverTime":epoch_ms(), "checkId":state.check_id, "checkState":state.check_state, "checkDeadline":state.check_deadline,
        "checkValid":check_valid(state, epoch_ms()),
        "rules":{"finishWindow":state.finish_window,"nextRace":state.next_race,"then":state.then},
        "rotation":state.rotation,"trackLimits":state.track_limits,"pickOnly":state.pick_only,
        "lastRace":if state.last_race_id == 0 { Value::Null } else { json!({"id":state.last_race_id,
            "track":state.last_race_track,"entrants":state.last_race_entrants}) }
    })
}

fn broadcast_room(state: &Room) {
    broadcast(state, Outbound::Text(room_message(state).to_string()), None);
}

fn hello_with_peer(
    state: &mut Room,
    text: &str,
    tx: &SyncSender<Outbound>,
    stopped: Arc<AtomicBool>,
    peer_ip: &str,
) -> Result<(u64, u64), ()> {
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            send_json(
                tx,
                json!({"type":"error","code":"bad_message","message":"Invalid hello."}),
            );
            return Err(());
        }
    };
    if msg["type"] != "hello" {
        send_json(
            tx,
            json!({"type":"error","code":"bad_message","message":"Send hello first."}),
        );
        return Err(());
    }
    if msg["protocol"].as_u64() != Some(PROTOCOL) {
        send_json(
            tx,
            json!({"type":"error","code":"protocol_mismatch","message":"This LAN game uses a different protocol version."}),
        );
        return Err(());
    }
    let version = msg["version"].as_str().unwrap_or_default();
    let iso = msg["isoSha256"].as_str().unwrap_or_default();
    let name: String = msg["name"]
        .as_str()
        .unwrap_or("Rider")
        .chars()
        .take(24)
        .collect();
    if version.is_empty() || iso.len() != 64 || name.is_empty() {
        send_json(
            tx,
            json!({"type":"error","code":"bad_message","message":"Invalid player details."}),
        );
        return Err(());
    }
    if let (Some(reconnect_id), Some(token)) =
        (msg["memberId"].as_u64(), msg["rejoinToken"].as_str())
    {
        if let Some(i) = member_index(state, reconnect_id) {
            let valid_window = state.members[i]
                .disconnected_until
                .is_none_or(|until| Instant::now() <= until);
            let valid = valid_window
                && version == state.version
                && iso == state.iso
                && token.len() == 64
                && token.bytes().all(|b| b.is_ascii_hexdigit())
                && token_matches(&state.members[i].token_hash, token.as_bytes());
            if valid {
                let (hash, next_token) = new_token().map_err(|_| ())?;
                state.members[i].stopped.store(true, Ordering::Release);
                state.members[i].token_hash = hash;
                state.members[i].generation = state.members[i].generation.wrapping_add(1).max(1);
                state.members[i].disconnected_until = None;
                state.members[i].last_check_id = 0;
                state.members[i].last_check_ok = false;
                state.members[i].tx = tx.clone();
                state.members[i].stopped = stopped.clone();
                let id = state.members[i].id;
                let generation = state.members[i].generation;
                send_json(
                    tx,
                    json!({"type":"welcome","protocol":PROTOCOL,"code":"LAN","memberId":id,
                    "host":state.host_id==Some(id),"rejoinToken":next_token,"serverTime":epoch_ms(),"transport":"lan","features":["race_go"],
                    "race":if state.started { json!({"id":state.race_id,"track":state.track,"startAt":state.start_at,"raceGoAt":state.race_go_at}) } else { Value::Null }}),
                );
                if state.started
                    && state.members[i].slot.is_some()
                    && state.members[i].race_state == "racing"
                {
                    let slot = state.members[i].slot.unwrap_or(0);
                    let players: Vec<Value> = state
                        .members
                        .iter()
                        .filter_map(|m| {
                            m.slot.map(|s| json!({
                        "slot":s,"memberId":m.id,"name":m.name,"rider":m.rider,"bike":m.bike
                    }))
                        })
                        .collect();
                    queue(&state.members[i], Outbound::Text(json!({"type":"start","yourSlot":slot,"track":state.track,"laps":state.laps,
                        "seed":state.seed,"serverTime":epoch_ms(),"startAt":state.start_at,"players":players}).to_string()));
                    if let Some(at) = state.race_go_at {
                        queue(
                            &state.members[i],
                            Outbound::Text(
                                json!({"type":"race_go","race":state.start_at,"at":at}).to_string(),
                            ),
                        );
                    }
                }
                broadcast_room(state);
                return Ok((id, generation));
            }
        }
    }
    if msg.get("memberId").is_some() || msg.get("rejoinToken").is_some() {
        send_json(
            tx,
            json!({"type":"error","code":"reconnect_expired","message":"The LAN reconnect window expired; join the room again."}),
        );
        let _ = tx.try_send(Outbound::Close(4004, "LAN reconnect expired.".into()));
        return Err(());
    }
    if state.closed || (state.started && state.host_id.is_some()) {
        send_json(
            tx,
            json!({"type":"error","code":"room_started","message":"A LAN race is already in progress."}),
        );
        return Err(());
    }
    if let Some(host) = state.host_id {
        if version != state.version {
            send_json(
                tx,
                json!({"type":"error","code":"version_mismatch","message":"All LAN players need the same game version."}),
            );
            return Err(());
        }
        if iso != state.iso {
            send_json(
                tx,
                json!({"type":"error","code":"iso_mismatch","message":"All LAN players need the same game disc files."}),
            );
            return Err(());
        }
        if state.members.len() >= state.max_players {
            send_json(
                tx,
                json!({"type":"error","code":"room_full","message":"This LAN room is full."}),
            );
            return Err(());
        }
        if let Err(code) = pin_admission(
            state,
            &peer_ip.to_string(),
            msg["pin"].as_str().unwrap_or(""),
            Instant::now(),
        ) {
            let message = if code == "pin_throttled" {
                "Too many incorrect PIN attempts; try again shortly."
            } else {
                "A valid room PIN is required."
            };
            send_json(tx, json!({"type":"error","code":code,"message":message}));
            return Err(());
        }
        let id = state.next_id;
        state.next_id += 1;
        let (guest_hash, guest_token) = new_token().map_err(|_| ())?;
        state.members.push(Member {
            id,
            name,
            ready: false,
            choosing: false,
            rider: 0,
            bike: 0,
            rtt_ms: None,
            tx: tx.clone(),
            stopped,
            slot: None,
            race_state: String::new(),
            dnf_reason: String::new(),
            finish_ticks: None,
            place: 0,
            token_hash: guest_hash,
            generation: 1,
            disconnected_until: None,
            last_check_id: 0,
            last_check_ok: false,
        });
        send_json(
            tx,
            json!({"type":"welcome","protocol":PROTOCOL,"code":"LAN","memberId":id,"host":false,"rejoinToken":guest_token,
            "serverTime":epoch_ms(), "checkId":state.check_id, "checkState":state.check_state, "checkDeadline":state.check_deadline,"transport":"lan","features":["race_go"]}),
        );
        broadcast_room(state);
        let _ = host;
        return Ok((id, 1));
    }
    state.version = version.to_string();
    state.iso = iso.to_string();
    state.room_name = name.clone();
    let id = state.next_id;
    state.next_id += 1;
    state.host_id = Some(id);
    let (host_hash, host_token) = new_token().map_err(|_| ())?;
    state.members.push(Member {
        id,
        name,
        ready: false,
        choosing: false,
        rider: 0,
        bike: 0,
        rtt_ms: None,
        tx: tx.clone(),
        stopped,
        slot: None,
        race_state: String::new(),
        dnf_reason: String::new(),
        finish_ticks: None,
        place: 0,
        token_hash: host_hash,
        generation: 1,
        disconnected_until: None,
        last_check_id: 0,
        last_check_ok: false,
    });
    send_json(
        tx,
        json!({"type":"welcome","protocol":PROTOCOL,"code":"LAN","memberId":id,"host":true,"rejoinToken":host_token,
        "serverTime":epoch_ms(), "checkId":state.check_id, "checkState":state.check_state, "checkDeadline":state.check_deadline,"transport":"lan","features":["race_go"]}),
    );
    broadcast_room(state);
    Ok((id, 1))
}

fn hello(
    state: &mut Room,
    text: &str,
    tx: &SyncSender<Outbound>,
    stopped: Arc<AtomicBool>,
) -> Result<(u64, u64), ()> {
    hello_with_peer(state, text, tx, stopped, "unknown")
}

fn member_index(state: &Room, id: u64) -> Option<usize> {
    state.members.iter().position(|m| m.id == id)
}

fn text_message(state: &mut Room, id: u64, generation: u64, text: &str) -> bool {
    if text.len() > 16_384 {
        return false;
    }
    let msg: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let kind = msg["type"].as_str().unwrap_or("");
    let Some(at) = member_index(state, id) else {
        return false;
    };
    if state.members[at].generation != generation
        || state.members[at].stopped.load(Ordering::Acquire)
    {
        return false;
    }
    let is_host = state.host_id == Some(id);
    match kind {
        "ping" => {
            if let Some(t) = msg["t"].as_f64() {
                queue(
                    &state.members[at],
                    Outbound::Text(
                        json!({"type":"pong","t":t,"serverTime":epoch_ms()}).to_string(),
                    ),
                );
            }
        }
        "pong" | "leave" => {
            if kind == "leave" {
                return false;
            }
        }
        "pin" => {
            if !is_host {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_host","message":"Only the host can change the room PIN."}).to_string()));
                return true;
            }
            if state.started {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"room_started","message":"The race is running."}).to_string()));
                return true;
            }
            let pin = msg["pin"].as_str().unwrap_or("");
            if pin.is_empty() {
                state.pin_salt = None;
                state.pin_verifier = None;
            } else if pin_valid(pin) {
                let mut salt = [0; 16];
                if SystemRandom::new().fill(&mut salt).is_ok() {
                    state.pin_verifier = Some(pin_verifier(&salt, pin));
                    state.pin_salt = Some(salt);
                }
            } else {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"PIN must be 4–8 ASCII digits."}).to_string()));
                return true;
            }
            broadcast_room(state);
        }
        "settings" => {
            if !is_host {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_host","message":"Only the host can change room settings."}).to_string()));
                return true;
            }
            if state.started {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"room_started","message":"The race is running."}).to_string()));
                return true;
            }
            let old_race_settings = (
                state.track,
                state.laps,
                state.seed,
                state.max_players,
                state.finish_window,
                state.next_race.clone(),
                state.then.clone(),
                state.rotation.clone(),
                state.track_limits.clone(),
                state.pick_only.clone(),
            );
            if msg.get("track").is_some() && msg["track"].as_u64().is_none_or(|v| v > 255)
                || msg.get("laps").is_some() && msg["laps"].as_u64().is_none_or(|v| v > 255)
                || msg.get("seed").is_some()
                    && !msg["seed"].is_null()
                    && msg["seed"].as_u64().is_none_or(|v| v > u32::MAX as u64)
            {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid LAN room setting."}).to_string()));
                return true;
            }
            if let Some(v) = msg["maxPlayers"].as_u64() {
                if !(2..=MAX_PLAYERS as u64).contains(&v) || v < state.members.len() as u64 {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid room capacity."}).to_string()));
                    return true;
                }
                state.max_players = v as usize;
            }
            if let Some(v) = msg["finishWindow"].as_u64() {
                if ![0, 30, 60, 120].contains(&v) {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid finish window."}).to_string()));
                    return true;
                }
                state.finish_window = v;
            }
            if let Some(v) = msg["nextRace"].as_str() {
                if !["auto", "host"].contains(&v) {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid next-race rule."}).to_string()));
                    return true;
                }
                /* LAN has no auto-start timer: the host explicitly starts every race. */
                state.next_race = "host".into();
            }
            if let Some(v) = msg["then"].as_str() {
                if !["same", "next", "random"].contains(&v) {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid track rule."}).to_string()));
                    return true;
                }
                state.then = v.to_string();
            }
            if msg.get("rotation").is_some() {
                let Some(rotation) = msg["rotation"].as_array() else {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid track rotation."}).to_string()));
                    return true;
                };
                let Some(limits) = msg["trackLimits"].as_array() else {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid track limits."}).to_string()));
                    return true;
                };
                let rotation: Option<Vec<u64>> = rotation.iter().map(Value::as_u64).collect();
                let limits: Option<Vec<u64>> = limits.iter().map(Value::as_u64).collect();
                let pick_only: Option<Vec<u64>> = if msg.get("pickOnly").is_none() {
                    Some(Vec::new())
                } else {
                    msg["pickOnly"]
                        .as_array()
                        .and_then(|a| a.iter().map(Value::as_u64).collect())
                };
                let (Some(rotation), Some(limits), Some(pick_only)) = (rotation, limits, pick_only)
                else {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid track rotation."}).to_string()));
                    return true;
                };
                if rotation.len() > 64
                    || rotation.len() != limits.len()
                    || limits.iter().any(|v| ![6, 8, 10].contains(v))
                    || !limits.contains(&10)
                    || pick_only.iter().any(|v| !rotation.contains(v))
                {
                    queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_value","message":"Invalid track rotation."}).to_string()));
                    return true;
                }
                state.rotation = rotation;
                state.track_limits = limits;
                state.pick_only = pick_only;
            }
            if let Some(v) = msg["track"].as_u64() {
                state.track = v;
            }
            if let Some(v) = msg["laps"].as_u64() {
                state.laps = v;
            }
            if msg["seed"].is_null() && msg.get("seed").is_some() {
                state.seed = None;
            } else if let Some(v) = msg["seed"].as_u64() {
                state.seed = Some(v);
            }
            if let Some(v) = msg["roomName"].as_str() {
                state.room_name = v.chars().take(32).collect();
            }
            let new_race_settings = (
                state.track,
                state.laps,
                state.seed,
                state.max_players,
                state.finish_window,
                state.next_race.clone(),
                state.then.clone(),
                state.rotation.clone(),
                state.track_limits.clone(),
                state.pick_only.clone(),
            );
            if old_race_settings != new_race_settings {
                for member in &mut state.members {
                    member.ready = false;
                    member.last_check_ok = false;
                }
                state.check_state = "idle".into();
                state.check_deadline = None;
                state.check_started_at = None;
            }
            broadcast_room(state);
        }
        "pick" => {
            let target = msg["member"].as_u64().unwrap_or(id);
            if target != id && !is_host {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_host","message":"Only the host can change another player's rider."}).to_string()));
                return true;
            }
            if let Some(i) = member_index(state, target) {
                if msg["choosing"].as_bool() == Some(true) {
                    state.members[i].choosing = true;
                    state.members[i].ready = false;
                } else {
                    if let Some(v) = msg["rider"].as_u64().filter(|v| *v <= 255) {
                        state.members[i].rider = v;
                    }
                    if let Some(v) = msg["bike"].as_u64().filter(|v| *v <= 255) {
                        state.members[i].bike = v;
                    }
                    state.members[i].choosing = false;
                }
                broadcast_room(state);
            }
        }
        "ready" => {
            if state.started {
                return true;
            }
            let ready = msg["ready"].as_bool().unwrap_or(false);
            if ready && state.members[at].choosing {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_ready","message":"Choose your rider first."}).to_string()));
                return true;
            }
            state.members[at].ready = ready;
            if ready {
                state.members[at].last_check_id = 0;
                state.members[at].last_check_ok = false;
            }
            broadcast_room(state);
        }
        "check_response" => {
            let check_id = msg["checkId"].as_u64().unwrap_or(0);
            if check_id == state.check_id
                && state.check_state == "pending"
                && epoch_ms() <= state.check_deadline.unwrap_or(0.0)
            {
                state.members[at].last_check_id = check_id;
                state.members[at].last_check_ok = msg["ok"].as_bool() == Some(true);
                if !state.members[at].last_check_ok {
                    state.check_state = "failed".into();
                } else {
                    let ready_count = state.members.iter().filter(|m| m.ready).count();
                    if ready_count > 0
                        && state
                            .members
                            .iter()
                            .filter(|m| m.ready)
                            .all(|m| m.last_check_id == check_id && m.last_check_ok)
                    {
                        state.check_state = "passed".into();
                    }
                }
                broadcast_room(state);
            }
        }
        "check_start" => {
            if !is_host || state.started {
                return true;
            }
            state.check_id = state.check_id.wrapping_add(1).max(1);
            state.check_state = "pending".into();
            let now = epoch_ms();
            state.check_started_at = Some(now);
            state.check_deadline = Some(now + CHECK_TIMEOUT_MS);
            for m in &mut state.members {
                m.last_check_id = 0;
                m.last_check_ok = false;
            }
            broadcast(state,Outbound::Text(json!({"type":"check","checkId":state.check_id,"deadline":state.check_deadline}).to_string()),None);
            broadcast_room(state);
        }
        "start" => {
            if !is_host {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_host","message":"Only the host can start the race."}).to_string()));
                return true;
            }
            if state.started {
                return true;
            }
            let racers: Vec<u64> = state
                .members
                .iter()
                .filter(|m| m.ready && !m.choosing && !m.stopped.load(Ordering::Acquire))
                .map(|m| m.id)
                .collect();
            let now = epoch_ms();
            if racers.is_empty() {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_ready","message":"At least one player must be ready."}).to_string()));
                return true;
            }
            if racers.len() > 1 && !check_valid(state, now) {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"connectivity_check_required","message":"Run a current connectivity check before starting."}).to_string()));
                return true;
            }
            let start_at = now + START_LEAD_MS;
            let seed = state
                .seed
                .unwrap_or_else(|| (now as u64 ^ id.rotate_left(11)) as u32 as u64);
            state.seed = Some(seed);
            state.started = true;
            state.start_at = Some(start_at);
            state.race_go_at = None;
            state.finish_ends_at = None;
            state.race_id = state.race_id.wrapping_add(1);
            for m in &mut state.members {
                m.slot = None;
                m.race_state.clear();
                m.finish_ticks = None;
                m.place = 0;
            }
            let mut members: Vec<(u64, String, u64, u64)> = Vec::new();
            for (slot, member_id) in racers.iter().enumerate() {
                if let Some(i) = member_index(state, *member_id) {
                    let m = &mut state.members[i];
                    m.slot = Some(slot as u8);
                    m.race_state = "racing".into();
                    m.finish_ticks = None;
                    m.place = 0;
                    members.push((m.id, m.name.clone(), m.rider, m.bike));
                }
            }
            let players: Vec<Value> = members.iter().enumerate().map(|(slot, (member_id, name, rider, bike))| {
                json!({"slot":slot,"memberId":member_id,"name":name,"rider":rider,"bike":bike})
            }).collect();
            for (slot, (member_id, _, _, _)) in members.iter().enumerate() {
                if let Some(i) = member_index(state, *member_id) {
                    queue(&state.members[i], Outbound::Text(json!({"type":"start","yourSlot":slot,"track":state.track,"laps":state.laps,
                        "seed":seed,"players":players,"serverTime":now,"startAt":start_at}).to_string()));
                }
            }
            broadcast_room(state);
        }
        "race_go" => {
            let race = msg["race"].as_f64().unwrap_or(0.0);
            let at_time = msg["at"].as_f64().unwrap_or(0.0);
            if state.started
                && state.members[at].slot.is_some()
                && state.members[at].race_state == "racing"
                && state.race_go_at.is_none()
                && Some(race) == state.start_at
                && at_time >= race
                && at_time <= race + RACE_GO_MAX_MS
            {
                state.race_go_at = Some(at_time);
                broadcast(
                    state,
                    Outbound::Text(json!({"type":"race_go","race":race,"at":at_time}).to_string()),
                    None,
                );
            }
        }
        "finish" => {
            let ticks = msg["ticks"].as_u64();
            let place = msg["place"].as_u64();
            if state.started
                && state.members[at].slot.is_some()
                && state.members[at].race_state == "racing"
            {
                if let Some(i) = member_index(state, id) {
                    if let (Some(ticks), Some(place)) =
                        (ticks, place.filter(|p| *p > 0 && *p <= 255))
                    {
                        state.members[i].finish_ticks = Some(ticks.min(u32::MAX as u64));
                        state.members[i].place = place;
                        state.members[i].race_state = "finished".into();
                        if state.finish_ends_at.is_none() && state.finish_window > 0 {
                            state.finish_ends_at =
                                Some(epoch_ms() + state.finish_window as f64 * 1000.0);
                        }
                        maybe_end_race(state);
                        broadcast_room(state);
                    }
                }
            }
        }
        "back" => {
            if state.started
                && state.members[at].slot.is_some()
                && state.members[at].race_state == "racing"
            {
                if let Some(i) = member_index(state, id) {
                    state.members[i].race_state = "dnf".into();
                    state.members[i].dnf_reason = "gave_up".into();
                    state.members[i].ready = false;
                    let slot = state.members[i].slot.unwrap_or(0);
                    broadcast(
                        state,
                        Outbound::Text(
                            json!({"type":"left","slot":slot,"memberId":id,"reason":"gave_up"})
                                .to_string(),
                        ),
                        None,
                    );
                    maybe_end_race(state);
                    broadcast_room(state);
                }
            }
        }
        "end" => {
            if is_host && state.started {
                end_race(state, true);
                broadcast_room(state);
            }
        }
        "kick" => {
            if !is_host {
                queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"not_host","message":"Only the host can remove a player."}).to_string()));
            } else if let Some(target) = msg["member"].as_u64() {
                if target != id {
                    if let Some(i) = member_index(state, target) {
                        let disconnected = state.members[i].disconnected_until.is_some()
                            || state.members[i].stopped.load(Ordering::Acquire);
                        if disconnected {
                            state.members[i].disconnected_until =
                                Some(Instant::now() - Duration::from_secs(1));
                        }
                        if state.started
                            && state.members[i].slot.is_some()
                            && state.members[i].race_state == "racing"
                        {
                            let slot = state.members[i].slot.unwrap_or(0);
                            state.members[i].ready = false;
                            state.members[i].race_state = "dnf".into();
                            state.members[i].dnf_reason = "kick".into();
                            broadcast(
                                state,
                                Outbound::Text(
                                    json!({"type":"left","slot":slot,"memberId":target,"reason":"kick"})
                                        .to_string(),
                                ),
                                None,
                            );
                            maybe_end_race(state);
                            broadcast_room(state);
                        }
                        if disconnected {
                            if let Some(i) = member_index(state, target) {
                                if !(state.started && state.members[i].slot.is_some()) {
                                    state.members.remove(i);
                                    broadcast_room(state);
                                }
                            }
                        } else if let Some(i) = member_index(state, target) {
                            queue(
                                &state.members[i],
                                Outbound::Close(
                                    4003,
                                    "The host removed you from the LAN room.".into(),
                                ),
                            );
                        }
                    }
                }
            }
        }
        _ => {
            queue(&state.members[at], Outbound::Text(json!({"type":"error","code":"bad_message","message":"Unsupported LAN room message."}).to_string()));
        }
    }
    true
}

fn maybe_end_race(state: &mut Room) {
    let has_racers = state.members.iter().any(|m| m.slot.is_some());
    let all_done = state
        .members
        .iter()
        .filter(|m| m.slot.is_some())
        .all(|m| m.race_state == "finished" || m.race_state == "dnf");
    if state.started && has_racers && all_done {
        end_race(state, false);
    }
}

fn finish_window_tick(room: &Arc<Mutex<Room>>) {
    let mut state = lock(room);
    let Some(deadline) = state.finish_ends_at else {
        return;
    };
    if !state.started || epoch_ms() < deadline {
        return;
    }
    let mut timed_out = Vec::new();
    for m in &mut state.members {
        if m.slot.is_some() && m.race_state == "racing" {
            let slot = m.slot.unwrap_or(0);
            m.race_state = "dnf".into();
            m.dnf_reason = "finish_timeout".into();
            timed_out.push((slot, m.id));
        }
    }
    for (slot, member_id) in timed_out {
        broadcast(
            &state,
            Outbound::Text(
                json!({"type":"left","slot":slot,"memberId":member_id,"reason":"finish_timeout"})
                    .to_string(),
            ),
            None,
        );
    }
    maybe_end_race(&mut state);
    broadcast_room(&state);
}

fn end_race(state: &mut Room, voided: bool) {
    if state.race_id != 0 {
        state.last_race_id = state.race_id;
        state.last_race_track = state.track;
        state.last_race_entrants = state
            .members
            .iter()
            .filter(|m| m.slot.is_some())
            .map(|m| {
                let mut entrant = json!({"id":m.id,"name":m.name,"rider":m.rider,"place":m.place});
                if let Some(ticks) = m.finish_ticks {
                    entrant["ticks"] = json!(ticks);
                }
                if !m.dnf_reason.is_empty() {
                    entrant["dnfReason"] = json!(m.dnf_reason);
                }
                entrant
            })
            .collect();
    }
    broadcast(
        state,
        Outbound::Text(
            json!({"type":"ended","reason":if voided {"void"} else {"done"}}).to_string(),
        ),
        None,
    );
    state.started = false;
    if !voided {
        state.track = rotate_track(state, state.track, state.race_id);
    }
    state.start_at = None;
    state.race_go_at = None;
    state.finish_ends_at = None;
    for m in &mut state.members {
        m.slot = None;
        m.ready = false;
        m.race_state.clear();
        m.dnf_reason.clear();
    }
    let now = Instant::now();
    state.members.retain(|m| {
        !m.stopped.load(Ordering::Acquire) || m.disconnected_until.is_some_and(|until| until > now)
    });
}

fn binary_message(state: &Room, id: u64, generation: u64, data: &[u8]) {
    if data.len() < 2 || data.len() > MAX_FRAME {
        return;
    }
    let Some(i) = member_index(state, id) else {
        return;
    };
    if state.members[i].generation != generation || state.members[i].stopped.load(Ordering::Acquire)
    {
        return;
    }
    match data[0] {
        3 if data.len() == 2 + std::mem::size_of::<f64>() && state.started => {
            let mut frame = data.to_vec();
            frame[0] = 4;
            frame[1] = state.members[i].slot.unwrap_or(0);
            frame.extend_from_slice(&epoch_ms().to_ne_bytes());
            queue(&state.members[i], Outbound::Binary(frame));
        }
        5 | 6 if state.started && state.members[i].slot.is_some() => {
            let mut frame = data.to_vec();
            frame[1] = state.members[i].slot.unwrap_or(0);
            broadcast(state, Outbound::Binary(frame), Some(id));
        }
        _ => {}
    }
}

fn remove_member(
    room: &Arc<Mutex<Room>>,
    id: u64,
    generation: u64,
    server_stop: &AtomicBool,
    explicit: bool,
) {
    let mut state = lock(room);
    let Some(i) = member_index(&state, id) else {
        return;
    };
    /* A superseded socket may finish teardown after its replacement has joined. */
    if state.members[i].generation != generation {
        return;
    }
    let is_host = state.host_id == Some(id);
    let shutdown = explicit || server_stop.load(Ordering::Acquire) || state.closed;
    state.members[i].stopped.store(true, Ordering::Release);
    if !shutdown {
        if state.members[i].disconnected_until.is_none() {
            state.members[i].disconnected_until = Some(Instant::now() + RECONNECT_GRACE);
            state.members[i].last_check_id = 0;
            state.members[i].last_check_ok = false;
            broadcast_room(&state);
        }
        return;
    }

    let mut host_left = false;
    if is_host {
        state.closed = true;
        state.members.remove(i);
        for peer in &state.members {
            queue(
                peer,
                Outbound::Text(
                    json!({"type":"error","code":"room_not_found","message":"The LAN host left."})
                        .to_string(),
                ),
            );
            peer.stopped.store(true, Ordering::Release);
        }
        host_left = true;
    } else if state.started && state.members[i].slot.is_some() {
        let slot = state.members[i].slot.unwrap_or(0);
        state.members[i].ready = false;
        state.members[i].disconnected_until = Some(Instant::now() - Duration::from_secs(1));
        state.members[i].last_check_id = 0;
        state.members[i].last_check_ok = false;
        if state.members[i].race_state == "racing" {
            state.members[i].race_state = "dnf".into();
            state.members[i].dnf_reason = if explicit { "leave" } else { "disconnect" }.into();
            broadcast(
                &state,
                Outbound::Text(
                    json!({"type":"left","slot":slot,"memberId":id,
                        "reason":state.members[i].dnf_reason})
                    .to_string(),
                ),
                None,
            );
        }
        maybe_end_race(&mut state);
        broadcast_room(&state);
    } else {
        state.members.remove(i);
        broadcast_room(&state);
    }
    drop(state);
    if host_left {
        server_stop.store(true, Ordering::Release);
    }
}

fn check_timeout_tick(room: &Arc<Mutex<Room>>) {
    let mut state = lock(room);
    if state.check_state == "pending"
        && state
            .check_deadline
            .is_some_and(|deadline| epoch_ms() > deadline)
    {
        state.check_state = "timeout".into();
        broadcast_room(&state);
    }
}

fn expire_disconnected(room: &Arc<Mutex<Room>>, server_stop: &AtomicBool) {
    let now = Instant::now();
    let mut state = lock(room);
    if state.closed {
        return;
    }
    let host_expired = state.members.iter().any(|m| {
        state.host_id == Some(m.id) && m.disconnected_until.is_some_and(|until| until <= now)
    });
    if host_expired {
        state.closed = true;
        for peer in &state.members {
            queue(
                peer,
                Outbound::Text(json!({"type":"error","code":"room_not_found","message":"The LAN host did not reconnect in time."}).to_string()),
            );
            peer.stopped.store(true, Ordering::Release);
        }
        drop(state);
        server_stop.store(true, Ordering::Release);
        return;
    }

    let expired: Vec<u64> = state
        .members
        .iter()
        .filter(|m| state.host_id != Some(m.id))
        .filter(|m| m.disconnected_until.is_some_and(|until| until <= now))
        .filter(|m| !(state.started && m.slot.is_some() && m.race_state != "racing"))
        .map(|m| m.id)
        .collect();
    if expired.is_empty() {
        return;
    }
    let mut departed = Vec::new();
    for id in expired {
        let Some(i) = member_index(&state, id) else {
            continue;
        };
        if state.started && state.members[i].slot.is_some() {
            let slot = state.members[i].slot.unwrap_or(0);
            state.members[i].ready = false;
            state.members[i].race_state = "dnf".into();
            state.members[i].dnf_reason = "disconnect".into();
            state.members[i].last_check_id = 0;
            state.members[i].last_check_ok = false;
            departed.push((slot, id));
        } else {
            state.members.remove(i);
        }
    }
    for (slot, id) in departed {
        broadcast(
            &state,
            Outbound::Text(
                json!({"type":"left","slot":slot,"memberId":id,"reason":"disconnect"}).to_string(),
            ),
            None,
        );
    }
    maybe_end_race(&mut state);
    broadcast_room(&state);
}

fn is_timed_out(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted))
}

fn peer_main(
    stream: TcpStream,
    peer: String,
    room: Arc<Mutex<Room>>,
    server_stop: Arc<AtomicBool>,
) {
    if let Err(e) = peer_run(stream, &peer, room, server_stop) {
        eprintln!("net: LAN peer {peer} closed ({e})");
    }
}

fn drain_stopped_outbound(
    ws: &mut tungstenite::WebSocket<TcpStream>,
    rx: &mpsc::Receiver<Outbound>,
    explicit_leave: &mut bool,
) -> Result<(), String> {
    loop {
        match rx.try_recv() {
            Ok(Outbound::Text(text)) => ws
                .send(Message::Text(Utf8Bytes::from(text)))
                .map_err(|e| e.to_string())?,
            Ok(Outbound::Binary(bytes)) => ws
                .send(Message::Binary(Bytes::from(bytes)))
                .map_err(|e| e.to_string())?,
            Ok(Outbound::Close(code, reason)) => {
                let frame = tungstenite::protocol::frame::CloseFrame {
                    code: tungstenite::protocol::frame::coding::CloseCode::from(code),
                    reason: Utf8Bytes::from(reason),
                };
                ws.send(Message::Close(Some(frame)))
                    .map_err(|e| e.to_string())?;
                *explicit_leave |= code == 4003;
            }
            Err(_) => break,
        }
    }
    ws.flush().map_err(|e| e.to_string())
}

fn peer_run(
    stream: TcpStream,
    peer: &str,
    room: Arc<Mutex<Room>>,
    server_stop: Arc<AtomicBool>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .map_err(|e| e.to_string())?;
    let mut ws = tungstenite::accept(stream).map_err(|e| format!("WebSocket handshake: {e}"))?;
    ws.get_ref()
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::sync_channel(OUTBOUND_QUEUE);
    let stopped = Arc::new(AtomicBool::new(false));
    let mut member: Option<(u64, u64)> = None;
    let mut close_after_flush = false;
    let mut explicit_leave = false;
    let outcome = (|| -> Result<(), String> {
        loop {
            if server_stop.load(Ordering::Acquire) || stopped.load(Ordering::Acquire) {
                drain_stopped_outbound(&mut ws, &rx, &mut explicit_leave)?;
                break;
            }
            finish_window_tick(&room);
            check_timeout_tick(&room);
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let parsed: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::Null);
                    let is_leave = parsed["type"] == "leave";
                    let mut state = lock(&room);
                    if let Some((id, generation)) = member {
                        let active = member_index(&state, id).is_some_and(|i| {
                            state.members[i].generation == generation
                                && !state.members[i].stopped.load(Ordering::Acquire)
                                && state.members[i].disconnected_until.is_none()
                        });
                        if !active || !text_message(&mut state, id, generation, text.as_str()) {
                            explicit_leave |= is_leave && active;
                            break;
                        }
                    } else {
                        match hello_with_peer(
                            &mut state,
                            text.as_str(),
                            &tx,
                            stopped.clone(),
                            peer.split(':').next().unwrap_or("unknown"),
                        ) {
                            Ok(identity) => member = Some(identity),
                            Err(()) => close_after_flush = true,
                        }
                    }
                }
                Ok(Message::Binary(data)) => {
                    if let Some((id, generation)) = member {
                        binary_message(&lock(&room), id, generation, data.as_ref());
                    } else {
                        let _ = tx.try_send(Outbound::Close(4000, "Send hello first.".into()));
                        close_after_flush = true;
                    }
                }
                Ok(Message::Ping(data)) => {
                    ws.send(Message::Pong(data)).map_err(|e| e.to_string())?;
                }
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) if is_timed_out(&e) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break
                }
                Err(e) => return Err(e.to_string()),
            }
            loop {
                match rx.try_recv() {
                    Ok(Outbound::Text(text)) => ws
                        .send(Message::Text(Utf8Bytes::from(text)))
                        .map_err(|e| e.to_string())?,
                    Ok(Outbound::Binary(bytes)) => ws
                        .send(Message::Binary(Bytes::from(bytes)))
                        .map_err(|e| e.to_string())?,
                    Ok(Outbound::Close(code, reason)) => {
                        let frame = tungstenite::protocol::frame::CloseFrame {
                            code: tungstenite::protocol::frame::coding::CloseCode::from(code),
                            reason: Utf8Bytes::from(reason),
                        };
                        let _ = ws.send(Message::Close(Some(frame)));
                        explicit_leave |= code == 4003;
                        close_after_flush = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        close_after_flush = true;
                        break;
                    }
                }
            }
            let _ = ws.flush();
            if close_after_flush {
                break;
            }
        }
        Ok(())
    })();
    if let Some((id, generation)) = member {
        remove_member(&room, id, generation, &server_stop, explicit_leave);
    }
    let _ = ws.close(None);
    outcome
}

fn copy_out(text: &str, out: *mut c_char, cap: usize) -> bool {
    if out.is_null() || cap == 0 || text.len() + 1 > cap {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out.cast::<u8>(), text.len());
        *out.add(text.len()) = 0;
    }
    true
}

/// Start an offline host room. `url_out` is the host's local WebSocket URL and `address_out` is the
/// LAN address:port that guests enter. Returns 1 on success, 0 on a visible setup error.
#[no_mangle]
pub unsafe extern "C" fn lan_host_start(
    url_out: *mut c_char,
    url_cap: usize,
    address_out: *mut c_char,
    address_cap: usize,
) -> i32 {
    let ip = match local_ipv4() {
        Ok(ip) => ip,
        Err(e) => {
            eprintln!("net: LAN host: {e}");
            return 0;
        }
    };
    let server = match start_server(ip, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("net: LAN host: {e}");
            return 0;
        }
    };
    let port = server
        .address
        .rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or_default();
    let url = format!("ws://127.0.0.1:{port}/");
    if !copy_out(&url, url_out, url_cap) || !copy_out(&server.address, address_out, address_cap) {
        server.stop.store(true, Ordering::Release);
        if let Some(thread) = server.thread {
            let _ = thread.join();
        }
        eprintln!("net: LAN host: output buffer too small");
        return 0;
    }
    let mut slot = lock(server_slot());
    if slot.is_some() {
        server.stop.store(true, Ordering::Release);
        if let Some(thread) = server.thread {
            let _ = thread.join();
        }
        eprintln!("net: LAN host: a local room is already running");
        return 0;
    }
    *slot = Some(server);
    1
}

/// Start a room for a peer's Tailscale IPv4 address; the listener itself is wildcard-bound.
#[no_mangle]
pub unsafe extern "C" fn lan_host_start_tailnet(
    address: *const c_char,
    url_out: *mut c_char,
    url_cap: usize,
    address_out: *mut c_char,
    address_cap: usize,
) -> i32 {
    if address.is_null() {
        return 0;
    }
    let text = unsafe { CStr::from_ptr(address) }.to_string_lossy();
    let Ok(ip) = text.trim().parse::<Ipv4Addr>() else {
        return 0;
    };
    if !is_tailnet_ipv4(ip) {
        return 0;
    }
    cancel_discovery_scan();
    let server = match start_server(ip, true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("net: Tailscale host: {e}");
            return 0;
        }
    };
    let port = server
        .address
        .rsplit_once(':')
        .map(|(_, p)| p)
        .unwrap_or_default();
    let url = format!("ws://127.0.0.1:{port}/");
    if !copy_out(&url, url_out, url_cap) || !copy_out(&server.address, address_out, address_cap) {
        server.stop.store(true, Ordering::Release);
        if let Some(thread) = server.thread {
            let _ = thread.join();
        }
        eprintln!("net: Tailscale host: output buffer too small");
        return 0;
    }
    let mut slot = lock(server_slot());
    if slot.is_some() {
        server.stop.store(true, Ordering::Release);
        if let Some(thread) = server.thread {
            let _ = thread.join();
        }
        eprintln!("net: Tailscale host: a local room is already running");
        return 0;
    }
    *slot = Some(server);
    drop(slot);
    cancel_discovery_scan();
    1
}

#[no_mangle]
pub unsafe extern "C" fn lan_interfaces(out: *mut LanInterface, cap: i32) -> i32 {
    if cap < 0 || (cap > 0 && out.is_null()) {
        return -1;
    }
    let selected = lock(server_slot())
        .as_ref()
        .and_then(|s| s.address.split(':').next().and_then(|x| x.parse().ok()));
    let list = local_interfaces();
    let n = list.len().min(cap as usize);
    if n > 0 {
        for (i, (name, ip, wifi)) in list.iter().take(n).enumerate() {
            unsafe {
                *out.add(i) = LanInterface {
                    name: c_array(name),
                    address: c_array(&ip.to_string()),
                    is_wifi: *wifi as i32,
                    selected: (Some(*ip) == selected) as i32,
                };
            }
        }
    }
    list.len() as i32
}
#[no_mangle]
pub unsafe extern "C" fn lan_host_select_ip(address: *const c_char) -> i32 {
    if address.is_null() {
        return 0;
    }
    let text = unsafe { CStr::from_ptr(address) }.to_string_lossy();
    let Ok(ip) = text.trim().parse::<Ipv4Addr>() else {
        return 0;
    };
    if !local_interfaces().iter().any(|(_, x, _)| *x == ip) {
        return 0;
    }
    let mut slot = lock(server_slot());
    let Some(s) = slot.as_mut() else {
        return 0;
    };
    if s.tailnet {
        return 0;
    }
    let port = s.address.rsplit_once(':').map(|(_, p)| p).unwrap_or("");
    s.address = format!("{ip}:{port}");
    *lock(&s.advertised) = s.address.clone();
    1
}
#[no_mangle]
pub unsafe extern "C" fn lan_host_refresh_address(out: *mut c_char, cap: usize) -> i32 {
    let interfaces = local_interfaces();
    let mut slot = lock(server_slot());
    let Some(s) = slot.as_mut() else {
        return 0;
    };
    if s.tailnet {
        return if copy_out(&s.address, out, cap) { 1 } else { 0 };
    }
    let port = s
        .address
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(0);
    let Some(address) = live_host_address(&s.address, port, &interfaces, local_ipv4().ok()) else {
        lock(&s.advertised).clear();
        return 0;
    };
    s.address = address;
    *lock(&s.advertised) = s.address.clone();
    if copy_out(&s.address, out, cap) {
        1
    } else {
        0
    }
}

/// Validate an IPv4:port typed by a guest and return the corresponding plain local WebSocket URL.
fn valid_guest_address(address: SocketAddrV4) -> bool {
    address.ip().is_private()
        && !address.ip().is_loopback()
        && !address.ip().is_unspecified()
        && !address.ip().is_broadcast()
        && !address.ip().is_multicast()
        && address.port() != 0
}

fn is_tailnet_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn valid_tailnet_guest_address(address: SocketAddrV4) -> bool {
    is_tailnet_ipv4(*address.ip()) && address.port() != 0
}

#[no_mangle]
pub unsafe extern "C" fn lan_guest_url(
    address: *const c_char,
    url_out: *mut c_char,
    url_cap: usize,
) -> i32 {
    if address.is_null() {
        return 0;
    }
    let text = unsafe { CStr::from_ptr(address) }.to_string_lossy();
    let Ok(address) = SocketAddrV4::from_str(text.trim()) else {
        return 0;
    };
    if !valid_guest_address(address) {
        return 0;
    }
    let ip = address.ip();
    let port = address.port();
    let url = format!("ws://{ip}:{port}/");
    if copy_out(&url, url_out, url_cap) {
        1
    } else {
        0
    }
}

/// Validate only a Tailscale shared-address IPv4:port and return its WebSocket URL.
#[no_mangle]
pub unsafe extern "C" fn lan_tailnet_guest_url(
    address: *const c_char,
    url_out: *mut c_char,
    url_cap: usize,
) -> i32 {
    if address.is_null() {
        return 0;
    }
    let text = unsafe { CStr::from_ptr(address) }.to_string_lossy();
    let Ok(address) = SocketAddrV4::from_str(text.trim()) else {
        return 0;
    };
    if !valid_tailnet_guest_address(address) {
        return 0;
    }
    let url = format!("ws://{}:{}/", address.ip(), address.port());
    if copy_out(&url, url_out, url_cap) {
        1
    } else {
        0
    }
}

/// Stop the host listener and close every client when the game leaves its LAN room.
#[no_mangle]
pub unsafe extern "C" fn lan_host_stop() {
    cancel_discovery_scan();
    let server = lock(server_slot()).take();
    if let Some(server) = server {
        server.stop.store(true, Ordering::Release);
        close_room(&server.room, "The LAN host stopped the room.");
        if let Some(thread) = server.thread {
            let _ = thread.join();
        }
        if let Some(thread) = server.discovery_thread {
            let _ = thread.join();
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn lan_discover_start() -> i32 {
    let server = lock(server_slot());
    if server.as_ref().is_some_and(|s| s.tailnet) {
        return 0;
    }
    let started = start_discovery_scan();
    drop(server);
    if started {
        1
    } else {
        0
    }
}
#[no_mangle]
pub unsafe extern "C" fn lan_discover_poll(out: *mut LanDiscoverResult, cap: i32) -> i32 {
    if cap < 0 {
        return -1;
    }
    let mut slot = lock(discover_slot());
    let Some(s) = slot.as_mut() else { return -1 };
    if s.working {
        return -2;
    };
    if s.failed {
        *slot = None;
        return -1;
    }
    if out.is_null() && !s.results.is_empty() {
        return -1;
    };
    let n = s.results.len().min(cap as usize);
    if n > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(s.results.as_ptr(), out, n);
        }
    }
    s.results.drain(..n);
    let result = n as i32;
    if s.results.is_empty() {
        *slot = None;
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::connect_ws;
    use tungstenite::protocol::WebSocket;

    fn wait_message(ws: &mut WebSocket<crate::stream::Stream>, want: &str) -> Value {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < until {
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let v: Value = serde_json::from_str(text.as_str()).unwrap();
                    if v["type"] == want {
                        return v;
                    }
                }
                Ok(_) => {}
                Err(e) if is_timed_out(&e) => {}
                Err(e) => panic!("WebSocket read failed: {e}"),
            }
        }
        panic!("timed out waiting for {want}");
    }

    fn wait_member_ready(ws: &mut WebSocket<crate::stream::Stream>, name: &str) {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < until {
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let room: Value = serde_json::from_str(text.as_str()).unwrap();
                    let ready = room["type"] == "room"
                        && room["members"].as_array().is_some_and(|members| {
                            members.iter().any(|member| {
                                member["name"].as_str() == Some(name)
                                    && member["ready"].as_bool() == Some(true)
                            })
                        });
                    if ready {
                        return;
                    }
                }
                Ok(_) => {}
                Err(e) if is_timed_out(&e) => {}
                Err(e) => panic!("WebSocket read failed: {e}"),
            }
        }
        panic!("timed out waiting for {name} to be ready");
    }

    fn wait_room_check_valid(ws: &mut WebSocket<crate::stream::Stream>) {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < until {
            match ws.read() {
                Ok(Message::Text(text)) => {
                    let room: Value = serde_json::from_str(text.as_str()).unwrap();
                    if room["type"] == "room" && room["checkValid"] == true {
                        return;
                    }
                }
                Ok(_) => {}
                Err(e) if is_timed_out(&e) => {}
                Err(e) => panic!("WebSocket read failed: {e}"),
            }
        }
        panic!("timed out waiting for LAN connectivity check");
    }

    fn wait_binary(ws: &mut WebSocket<crate::stream::Stream>) -> Vec<u8> {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < until {
            match ws.read() {
                Ok(Message::Binary(data)) => return data.to_vec(),
                Ok(_) => {}
                Err(e) if is_timed_out(&e) => {}
                Err(e) => panic!("WebSocket read failed: {e}"),
            }
        }
        panic!("timed out waiting for binary race frame");
    }

    fn connect(url: &str, name: &str) -> WebSocket<crate::stream::Stream> {
        let (mut ws, _tcp, _peek) = connect_ws(url).unwrap();
        let hello = json!({"type":"hello","protocol":4,"version":"test-build","clientVersion":"0.7.16",
            "isoSha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","name":name,
            "features":["race_go"]});
        ws.send(Message::Text(Utf8Bytes::from(hello.to_string())))
            .unwrap();
        assert_eq!(wait_message(&mut ws, "welcome")["type"], "welcome");
        wait_message(&mut ws, "room");
        ws
    }

    fn test_member(id: u64, name: &str, tx: SyncSender<Outbound>) -> Member {
        Member {
            id,
            name: name.into(),
            ready: false,
            choosing: false,
            rider: 0,
            bike: 0,
            rtt_ms: None,
            tx,
            stopped: Arc::new(AtomicBool::new(false)),
            slot: None,
            race_state: String::new(),
            dnf_reason: String::new(),
            finish_ticks: None,
            place: 0,
            token_hash: [0; 32],
            generation: 1,
            disconnected_until: None,
            last_check_id: 0,
            last_check_ok: false,
        }
    }

    #[test]
    fn local_room_joins_starts_and_relays_frames() {
        let s = start_server(Ipv4Addr::LOCALHOST, false).unwrap();
        let port = s.address.rsplit_once(':').unwrap().1;
        let url = format!("ws://127.0.0.1:{port}/");
        let stop = s.stop.clone();
        let accept = s.thread;
        let mut host = connect(&url, "Host");
        let mut guest = connect(&url, "Guest");
        assert_eq!(
            host.read()
                .unwrap()
                .into_text()
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .map(|v| v["type"].clone()),
            Some(json!("room"))
        );
        let mut set = json!({"type":"settings","track":7,"laps":2,"roomName":"LAN test","maxPlayers":6,
            "finishWindow":30,"nextRace":"host","then":"random","rotation":[7,8,9],"trackLimits":[10,8,6],"pickOnly":[8]}).to_string();
        host.send(Message::Text(Utf8Bytes::from(std::mem::take(&mut set))))
            .unwrap();
        let room_msg = wait_message(&mut guest, "room");
        assert_eq!(room_msg["settings"]["track"], 7);
        assert_eq!(room_msg["roomName"], "LAN test");
        assert_eq!(room_msg["maxPlayers"], 6);
        assert_eq!(room_msg["rules"]["finishWindow"], 30);
        assert_eq!(room_msg["rules"]["nextRace"], "host");
        assert_eq!(room_msg["rules"]["then"], "random");
        assert_eq!(room_msg["rotation"], json!([7, 8, 9]));
        assert_eq!(room_msg["trackLimits"], json!([10, 8, 6]));
        assert_eq!(room_msg["pickOnly"], json!([8]));
        host.send(Message::Text(Utf8Bytes::from(
            json!({"type":"pick","rider":2,"bike":1}).to_string(),
        )))
        .unwrap();
        host.send(Message::Text(Utf8Bytes::from(
            json!({"type":"ready","ready":true}).to_string(),
        )))
        .unwrap();
        wait_member_ready(&mut host, "Host");
        wait_member_ready(&mut guest, "Host");
        guest
            .send(Message::Text(Utf8Bytes::from(
                json!({"type":"pick","rider":4,"bike":1}).to_string(),
            )))
            .unwrap();
        guest
            .send(Message::Text(Utf8Bytes::from(
                json!({"type":"ready","ready":true}).to_string(),
            )))
            .unwrap();
        wait_member_ready(&mut guest, "Guest");
        wait_member_ready(&mut host, "Guest");
        host.send(Message::Text(Utf8Bytes::from(
            json!({"type":"check_start"}).to_string(),
        )))
        .unwrap();
        let host_check = wait_message(&mut host, "check");
        let guest_check = wait_message(&mut guest, "check");
        assert_eq!(host_check["checkId"], guest_check["checkId"]);
        for ws in [&mut host, &mut guest] {
            ws.send(Message::Text(Utf8Bytes::from(
                json!({"type":"check_response","checkId":host_check["checkId"],"ok":true})
                    .to_string(),
            )))
            .unwrap();
        }
        wait_room_check_valid(&mut host);
        wait_room_check_valid(&mut guest);
        host.send(Message::Text(Utf8Bytes::from(
            json!({"type":"start"}).to_string(),
        )))
        .unwrap();
        let hstart = wait_message(&mut host, "start");
        let gstart = wait_message(&mut guest, "start");
        assert_eq!(hstart["yourSlot"], 0);
        assert_eq!(gstart["yourSlot"], 1);
        let mut frame = vec![5, 0, 1, 2, 3, 4];
        host.send(Message::Binary(Bytes::from(frame.clone())))
            .unwrap();
        let got = wait_binary(&mut guest);
        assert_eq!(got, vec![5, 0, 1, 2, 3, 4]);
        frame[0] = 6;
        guest.send(Message::Binary(Bytes::from(frame))).unwrap();
        let got = wait_binary(&mut host);
        assert_eq!(got, vec![6, 1, 1, 2, 3, 4]);
        guest.send(Message::Text(Utf8Bytes::from(json!({"type":"race_go","race":hstart["startAt"],"at":hstart["startAt"].as_f64().unwrap()+1000.0}).to_string()))).unwrap();
        let go = wait_message(&mut host, "race_go");
        assert_eq!(go["race"], hstart["startAt"]);
        host.send(Message::Text(Utf8Bytes::from(
            json!({"type":"leave"}).to_string(),
        )))
        .unwrap();
        let stopped = wait_message(&mut guest, "error");
        assert_eq!(stopped["code"], "room_not_found");
        stop.store(true, Ordering::Release);
        if let Some(thread) = accept {
            let _ = thread.join();
        }
    }

    #[test]
    fn host_starts_only_ready_members() {
        let mut state = Room::default();
        state.host_id = Some(1);
        state.check_id = 1;
        state.check_state = "passed".into();
        state.check_started_at = Some(epoch_ms());
        let (tx0, rx0) = mpsc::sync_channel(16);
        let (tx1, rx1) = mpsc::sync_channel(16);
        let (tx2, rx2) = mpsc::sync_channel(16);
        let mut host = test_member(1, "Host", tx0);
        host.ready = true;
        host.last_check_id = 1;
        host.last_check_ok = true;
        let mut guest = test_member(2, "Guest", tx1);
        guest.ready = true;
        guest.last_check_id = 1;
        guest.last_check_ok = true;
        state.members = vec![host, guest, test_member(3, "Waiting", tx2)];
        assert!(text_message(&mut state, 1, 1, r#"{"type":"start"}"#));
        for rx in [&rx0, &rx1] {
            let Outbound::Text(text) = rx.try_recv().unwrap() else {
                panic!("expected start text")
            };
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["type"],
                "start"
            );
        }
        let Outbound::Text(snapshot) = rx2.try_recv().unwrap() else {
            panic!("expected room snapshot")
        };
        let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(snapshot["started"], true);
        assert_eq!(snapshot["members"][2]["inRace"], false);
        assert_eq!(snapshot["members"][0]["inRace"], true);
        assert_eq!(state.members[2].slot, None);
    }

    #[test]
    fn lan_host_can_start_a_solo_race_without_a_connectivity_check() {
        let mut state = Room::default();
        state.host_id = Some(1);
        let (tx, rx) = mpsc::sync_channel(16);
        let mut host = test_member(1, "Host", tx);
        host.ready = true;
        state.members = vec![host];

        assert!(text_message(&mut state, 1, 1, r#"{"type":"start"}"#));
        assert!(state.started);
        assert_eq!(state.members[0].slot, Some(0));
        let messages: Vec<Value> = rx
            .try_iter()
            .filter_map(|out| match out {
                Outbound::Text(text) => serde_json::from_str(&text).ok(),
                _ => None,
            })
            .collect();
        let start = messages
            .iter()
            .find(|message| message["type"] == "start")
            .unwrap();
        assert_eq!(start["yourSlot"], 0);
        assert_eq!(start["players"].as_array().unwrap().len(), 1);
        assert_eq!(start["players"][0]["memberId"], 1);
    }

    #[test]
    fn lan_start_requires_a_current_passed_check_from_every_ready_member() {
        let mut state = Room::default();
        state.host_id = Some(1);
        let (tx0, rx0) = mpsc::sync_channel(32);
        let (tx1, rx1) = mpsc::sync_channel(32);
        let mut host = test_member(1, "Host", tx0);
        host.ready = true;
        let mut guest = test_member(2, "Guest", tx1);
        guest.ready = true;
        state.members = vec![host, guest];

        assert!(text_message(&mut state, 1, 1, r#"{"type":"start"}"#));
        assert!(!state.started);
        assert!(rx0.try_iter().any(|out| matches!(out, Outbound::Text(text)
            if serde_json::from_str::<Value>(&text).is_ok_and(|v| v["code"] == "connectivity_check_required"))));

        assert!(text_message(&mut state, 1, 1, r#"{"type":"check_start"}"#));
        let id = state.check_id;
        assert_eq!(state.check_state, "pending");
        assert!(text_message(
            &mut state,
            1,
            1,
            &json!({"type":"check_response","checkId":id,"ok":true}).to_string()
        ));
        assert!(!check_valid(&state, epoch_ms()));
        assert!(text_message(
            &mut state,
            2,
            1,
            &json!({"type":"check_response","checkId":id,"ok":true}).to_string()
        ));
        assert!(check_valid(&state, epoch_ms()));
        assert!(text_message(&mut state, 1, 1, r#"{"type":"start"}"#));
        assert!(state.started);
        let _ = rx1.try_iter().collect::<Vec<_>>();
    }

    #[test]
    fn pending_lan_check_times_out_and_passed_check_expires() {
        let room = Arc::new(Mutex::new(Room::default()));
        {
            let mut state = lock(&room);
            state.check_id = 3;
            state.check_state = "pending".into();
            state.check_deadline = Some(epoch_ms() - 1.0);
        }
        check_timeout_tick(&room);
        {
            let mut state = lock(&room);
            assert_eq!(state.check_state, "timeout");
            assert!(!check_valid(&state, epoch_ms()));
            state.check_state = "passed".into();
            state.check_started_at = Some(epoch_ms() - CHECK_FRESH_MS - 1.0);
            assert!(!check_valid(&state, epoch_ms()));
        }
    }

    #[test]
    fn reconnect_replays_race_state_then_returns_to_lobby_safely() {
        let mut state = Room::default();
        state.host_id = Some(1);
        state.version = "test-build".into();
        state.iso = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
        state.started = true;
        state.race_id = 17;
        state.track = 9;
        state.laps = 2;
        state.seed = Some(123);
        state.start_at = Some(epoch_ms() + 1000.0);
        state.race_go_at = Some(epoch_ms() + 2000.0);
        let (host_tx, _host_rx) = mpsc::sync_channel(16);
        let (old_hash, token) = new_token().unwrap();
        let (guest_tx, _old_rx) = mpsc::sync_channel(16);
        let mut host = test_member(1, "Host", host_tx);
        host.slot = Some(0);
        host.race_state = "racing".into();
        host.rider = 1;
        host.bike = 2;
        let mut guest = test_member(2, "Guest", guest_tx);
        guest.token_hash = old_hash;
        guest.slot = Some(1);
        guest.race_state = "racing".into();
        guest.rider = 3;
        guest.bike = 4;
        guest.disconnected_until = Some(Instant::now() + RECONNECT_GRACE);
        guest.stopped.store(true, Ordering::Release);
        state.members = vec![host, guest];

        let (tx, rx) = mpsc::sync_channel(16);
        let new_stopped = Arc::new(AtomicBool::new(false));
        let hello_text = json!({"type":"hello","protocol":PROTOCOL,"version":"test-build",
            "isoSha256":state.iso,"name":"Guest","memberId":2,"rejoinToken":token})
        .to_string();
        assert_eq!(
            hello(&mut state, &hello_text, &tx, new_stopped.clone()).unwrap(),
            (2, 2)
        );
        assert_eq!(state.members[1].generation, 2);
        assert!(state.members[1].disconnected_until.is_none());
        assert!(!new_stopped.load(Ordering::Acquire));
        let replayed: Vec<Value> = rx
            .try_iter()
            .filter_map(|out| match out {
                Outbound::Text(text) => serde_json::from_str::<Value>(&text).ok(),
                _ => None,
            })
            .collect();
        assert_eq!(replayed[0]["type"], "welcome");
        let start = replayed.iter().find(|m| m["type"] == "start").unwrap();
        assert_eq!(start["yourSlot"], 1);
        assert_eq!(start["track"], 9);
        assert_eq!(start["laps"], 2);
        assert_eq!(start["seed"], 123);
        assert_eq!(start["players"].as_array().unwrap().len(), 2);
        assert!(replayed.iter().any(|m| m["type"] == "race_go"));
        let next_token = replayed[0]["rejoinToken"].as_str().unwrap().to_string();
        assert!(replayed[0]["rejoinToken"].as_str().is_some());
        assert!(replayed[0].get("token").is_none());

        let stop = AtomicBool::new(false);
        let room = Arc::new(Mutex::new(state));
        remove_member(&room, 2, 1, &stop, false);
        assert_eq!(lock(&room).members[1].generation, 2);
        remove_member(&room, 2, 2, &stop, false);
        assert!(lock(&room).members[1].disconnected_until.is_some());
        {
            let mut state = lock(&room);
            assert!(text_message(&mut state, 1, 1, r#"{"type":"end"}"#));
            assert!(!state.started);
        }

        let (tx2, rx2) = mpsc::sync_channel(16);
        let iso = lock(&room).iso.clone();
        let hello_after_end = json!({"type":"hello","protocol":PROTOCOL,"version":"test-build",
            "isoSha256":iso,"name":"Guest","memberId":2,"rejoinToken":next_token})
        .to_string();
        {
            let mut state = lock(&room);
            assert_eq!(
                hello(
                    &mut state,
                    &hello_after_end,
                    &tx2,
                    Arc::new(AtomicBool::new(false))
                )
                .unwrap(),
                (2, 3)
            );
        }
        let after_end: Vec<Value> = rx2
            .try_iter()
            .filter_map(|out| match out {
                Outbound::Text(text) => serde_json::from_str::<Value>(&text).ok(),
                _ => None,
            })
            .collect();
        assert_eq!(after_end[0]["type"], "welcome");
        assert!(after_end.iter().all(|m| m["type"] != "start"));
        remove_member(&room, 2, 2, &stop, false);
        assert_eq!(lock(&room).members[1].generation, 3);
        remove_member(&room, 2, 3, &stop, false);
        assert!(lock(&room).members[1].disconnected_until.is_some());
    }

    #[test]
    fn disconnected_guest_expires_and_disconnected_host_closes_the_room() {
        let room = Arc::new(Mutex::new(Room::default()));
        {
            let mut state = lock(&room);
            state.host_id = Some(1);
            let (tx0, _rx0) = mpsc::sync_channel(8);
            let (tx1, _rx1) = mpsc::sync_channel(8);
            state.members.push(test_member(1, "Host", tx0));
            let mut guest = test_member(2, "Guest", tx1);
            guest.disconnected_until = Some(Instant::now() - Duration::from_secs(1));
            guest.stopped.store(true, Ordering::Release);
            state.members.push(guest);
        }
        let stop = AtomicBool::new(false);
        expire_disconnected(&room, &stop);
        assert_eq!(lock(&room).members.len(), 1);

        {
            let mut state = lock(&room);
            state.members[0].disconnected_until = Some(Instant::now() - Duration::from_secs(1));
            state.members[0].stopped.store(true, Ordering::Release);
        }
        expire_disconnected(&room, &stop);
        assert!(stop.load(Ordering::Acquire));
        assert!(lock(&room).closed);
    }

    #[test]
    fn discovery_response_must_echo_the_scan_nonce() {
        assert_eq!(std::mem::size_of::<LanDiscoverResult>(), 136);
        assert_eq!(std::mem::offset_of!(LanDiscoverResult, max_players), 128);
        assert_eq!(std::mem::offset_of!(LanDiscoverResult, rtt_ms), 132);
        let nonce = [7u8; 16];
        let response = discovery_response(
            "192.168.1.42:1234",
            "LAN test",
            2,
            6,
            7,
            &nonce,
            &[0; 16],
            false,
        );
        let parsed = parse_discovery_response(&response, &nonce).unwrap();
        assert_eq!(parsed.players, 2);
        assert_eq!(parsed.max_players, 6);
        assert_eq!(parsed.track, 7);
        let wrong_nonce = [8u8; 16];
        assert!(parse_discovery_response(&response, &wrong_nonce).is_none());

        let full = discovery_response(
            "192.168.1.42:1234",
            "Full",
            6,
            6,
            7,
            &nonce,
            &[0; 16],
            false,
        );
        assert!(parse_discovery_response(&full, &nonce).is_none());
    }

    #[test]
    fn discovery_order_prefers_fast_links_then_more_open_seats() {
        let mut rooms = vec![
            LanDiscoverResult {
                address: c_array("192.168.1.3:1000"),
                room_name: c_array("A"),
                players: 4,
                track: 1,
                room_id: c_array("0000000000000001"),
                pin_required: 0,
                max_players: 6,
                rtt_ms: 40,
            },
            LanDiscoverResult {
                address: c_array("192.168.1.2:1000"),
                room_name: c_array("B"),
                players: 1,
                track: 1,
                room_id: c_array("0000000000000002"),
                pin_required: 0,
                max_players: 6,
                rtt_ms: 40,
            },
            LanDiscoverResult {
                address: c_array("192.168.1.1:1000"),
                room_name: c_array("C"),
                players: 1,
                track: 1,
                room_id: c_array("0000000000000003"),
                pin_required: 0,
                max_players: 6,
                rtt_ms: 20,
            },
        ];
        sort_discovery_results(&mut rooms);
        assert_eq!(rooms[0].room_name[0], 'C' as c_char);
        assert_eq!(rooms[1].room_name[0], 'B' as c_char);
        assert_eq!(rooms[2].room_name[0], 'A' as c_char);
    }

    #[test]
    fn discovery_only_lists_open_rooms_with_available_seats() {
        let mut state = Room::default();
        let (tx, _rx) = mpsc::sync_channel(8);
        state.host_id = Some(1);
        state.max_players = 2;
        state.members.push(test_member(1, "Host", tx));
        assert!(discovery_room_open(&state));

        state
            .members
            .push(test_member(2, "Guest", mpsc::sync_channel(8).0));
        assert!(!discovery_room_open(&state)); // full
        state.members.pop();
        state.started = true;
        assert!(!discovery_room_open(&state)); // active race
        state.started = false;
        state.closed = true;
        assert!(!discovery_room_open(&state)); // closed
    }

    #[test]
    fn next_rotation_skips_pick_only_and_handles_unlisted_current_track() {
        let mut state = Room::default();
        state.then = "next".into();
        state.rotation = vec![8, 9, 10];
        state.pick_only = vec![8];
        assert_eq!(rotate_track(&state, 8, 0), 9);
        assert_eq!(rotate_track(&state, 42, 0), 9);
        assert_eq!(rotate_track(&state, 10, 0), 9);
    }

    #[test]
    fn finish_window_marks_remaining_riders_and_saves_results() {
        let room = Arc::new(Mutex::new(Room::default()));
        let (tx0, rx0) = mpsc::sync_channel(16);
        let (tx1, rx1) = mpsc::sync_channel(16);
        {
            let mut state = lock(&room);
            state.host_id = Some(1);
            state.started = true;
            state.race_id = 9;
            state.finish_ends_at = Some(0.0);
            let mut first = test_member(1, "First", tx0);
            first.slot = Some(0);
            first.race_state = "finished".into();
            first.finish_ticks = Some(100);
            first.place = 1;
            let mut late = test_member(2, "Late", tx1);
            late.slot = Some(1);
            late.race_state = "racing".into();
            state.members = vec![first, late];
        }
        finish_window_tick(&room);
        let mut messages = Vec::new();
        for rx in [&rx0, &rx1] {
            for out in rx.try_iter() {
                if let Outbound::Text(text) = out {
                    if let Ok(message) = serde_json::from_str::<Value>(&text) {
                        messages.push(message);
                    }
                }
            }
        }
        assert!(messages
            .iter()
            .any(|m| m["type"] == "left" && m["reason"] == "finish_timeout"));
        assert!(messages.iter().any(|m| m["type"] == "ended"));
        assert!(messages.iter().any(|m| m["type"] == "room"
            && m["lastRace"]["id"] == 9
            && m["lastRace"]["entrants"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["dnfReason"] == "finish_timeout")));
        assert!(!lock(&room).started);
    }

    #[test]
    fn kick_revokes_a_disconnected_racers_token_and_keeps_the_result_reason() {
        let mut state = Room::default();
        state.host_id = Some(1);
        state.version = "test-build".into();
        state.iso = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
        state.started = true;
        state.race_id = 21;
        let (host_tx, _host_rx) = mpsc::sync_channel(16);
        let (old_hash, token) = new_token().unwrap();
        let (guest_tx, _guest_rx) = mpsc::sync_channel(16);
        let mut host = test_member(1, "Host", host_tx);
        host.slot = Some(0);
        host.race_state = "racing".into();
        let mut guest = test_member(2, "Guest", guest_tx);
        guest.token_hash = old_hash;
        guest.slot = Some(1);
        guest.race_state = "racing".into();
        guest.disconnected_until = Some(Instant::now() + RECONNECT_GRACE);
        guest.stopped.store(true, Ordering::Release);
        state.members = vec![host, guest];

        assert!(text_message(
            &mut state,
            1,
            1,
            r#"{"type":"kick","member":2}"#
        ));
        assert_eq!(state.members[1].race_state, "dnf");
        assert_eq!(state.members[1].dnf_reason, "kick");
        assert!(state.members[1]
            .disconnected_until
            .is_some_and(|until| until < Instant::now()));

        let iso = state.iso.clone();
        let rejoin = json!({"type":"hello","protocol":PROTOCOL,"version":"test-build",
            "isoSha256":iso,"name":"Guest","memberId":2,"rejoinToken":token})
        .to_string();
        let (tx, rx) = mpsc::sync_channel(8);
        assert!(hello(&mut state, &rejoin, &tx, Arc::new(AtomicBool::new(false))).is_err());
        let rejection: Vec<Value> = rx
            .try_iter()
            .filter_map(|out| match out {
                Outbound::Text(text) => serde_json::from_str::<Value>(&text).ok(),
                _ => None,
            })
            .collect();
        assert!(rejection
            .iter()
            .any(|m| m["type"] == "error" && m["code"] == "reconnect_expired"));

        assert!(text_message(
            &mut state,
            1,
            1,
            r#"{"type":"finish","ticks":100,"place":1}"#
        ));
        assert!(!state.started);
        assert!(state
            .last_race_entrants
            .iter()
            .any(|e| e["id"] == 2 && e["dnfReason"] == "kick"));
        assert!(state.members.iter().all(|m| m.id != 2));
    }

    #[test]
    fn voluntary_dnf_reason_is_retained_in_last_race_results() {
        let mut state = Room::default();
        state.host_id = Some(1);
        state.started = true;
        state.race_id = 22;
        let (host_tx, _host_rx) = mpsc::sync_channel(16);
        let (guest_tx, _guest_rx) = mpsc::sync_channel(16);
        let mut host = test_member(1, "Host", host_tx);
        host.slot = Some(0);
        host.race_state = "racing".into();
        let mut guest = test_member(2, "Guest", guest_tx);
        guest.slot = Some(1);
        guest.race_state = "racing".into();
        state.members = vec![host, guest];

        assert!(text_message(&mut state, 2, 1, r#"{"type":"back"}"#));
        assert_eq!(state.members[1].dnf_reason, "gave_up");
        assert!(text_message(
            &mut state,
            1,
            1,
            r#"{"type":"finish","ticks":100,"place":1}"#
        ));
        assert!(!state.started);
        assert!(state
            .last_race_entrants
            .iter()
            .any(|e| e["id"] == 2 && e["dnfReason"] == "gave_up"));
    }

    #[test]
    fn pin_admission_requires_and_checks_the_configured_pin() {
        let salt = [17u8; 16];
        let mut room = Room::default();
        room.pin_salt = Some(salt);
        room.pin_verifier = Some(pin_verifier(&salt, "2468"));
        let now = Instant::now();
        assert_eq!(
            pin_admission(&mut room, "192.168.1.8", "", now),
            Err("pin_required")
        );
        assert_eq!(
            pin_admission(&mut room, "192.168.1.8", "1111", now),
            Err("pin_invalid")
        );
        assert_eq!(pin_admission(&mut room, "192.168.1.8", "2468", now), Ok(()));
        assert_eq!(
            pin_admission(&mut room, "192.168.1.9", "", now),
            Err("pin_required")
        );
    }

    #[test]
    fn pin_throttle_survives_attempt_window_pruning_then_expires() {
        let salt = [23u8; 16];
        let mut room = Room::default();
        room.pin_salt = Some(salt);
        room.pin_verifier = Some(pin_verifier(&salt, "2468"));
        let start = Instant::now();
        for second in 0..5 {
            assert_eq!(
                pin_admission(
                    &mut room,
                    "192.168.1.8",
                    "0000",
                    start + Duration::from_secs(second)
                ),
                Err("pin_invalid")
            );
        }
        assert_eq!(
            pin_admission(
                &mut room,
                "192.168.1.8",
                "2468",
                start + Duration::from_secs(65)
            ),
            Err("pin_throttled")
        );
        assert_eq!(
            pin_admission(
                &mut room,
                "192.168.1.8",
                "0000",
                start + Duration::from_secs(95)
            ),
            Err("pin_invalid")
        );
    }

    #[test]
    fn guest_address_validation_requires_private_ipv4() {
        assert!(SocketAddrV4::from_str("192.168.1.42:1234").is_ok());
        assert!(SocketAddrV4::from_str("example.com:1234").is_err());
        assert!(SocketAddrV4::from_str("192.168.1.42").is_err());
        assert!(valid_guest_address("192.168.1.42:1234".parse().unwrap()));
        assert!(!valid_guest_address("127.0.0.1:1234".parse().unwrap()));
        assert!(!valid_guest_address("8.8.8.8:53".parse().unwrap()));
        assert!(!valid_guest_address("0.0.0.0:1".parse().unwrap()));
        assert!(!valid_guest_address("100.100.10.20:1234".parse().unwrap()));
    }

    #[test]
    fn tailnet_validation_accepts_only_shared_ipv4_addresses() {
        assert!(valid_tailnet_guest_address(
            "100.64.0.1:1234".parse().unwrap()
        ));
        assert!(valid_tailnet_guest_address(
            "100.127.255.254:48123".parse().unwrap()
        ));
        assert!(!valid_tailnet_guest_address(
            "100.63.255.255:1234".parse().unwrap()
        ));
        assert!(!valid_tailnet_guest_address(
            "100.128.0.1:1234".parse().unwrap()
        ));
        assert!(!valid_tailnet_guest_address(
            "192.168.1.42:1234".parse().unwrap()
        ));
        assert!(!valid_tailnet_guest_address(
            "8.8.8.8:1234".parse().unwrap()
        ));
        assert!(!valid_tailnet_guest_address(
            "100.100.1.2:0".parse().unwrap()
        ));
    }

    #[test]
    fn tailnet_listener_allows_self_and_tailnet_peers_only() {
        assert!(tailnet_peer_allowed("127.0.0.1:1234".parse().unwrap()));
        assert!(tailnet_peer_allowed("100.64.0.1:1234".parse().unwrap()));
        assert!(tailnet_peer_allowed(
            "100.127.255.254:1234".parse().unwrap()
        ));
        assert!(!tailnet_peer_allowed("192.168.1.42:1234".parse().unwrap()));
        assert!(!tailnet_peer_allowed("10.0.0.2:1234".parse().unwrap()));
        assert!(!tailnet_peer_allowed("8.8.8.8:1234".parse().unwrap()));
    }

    #[test]
    fn tailnet_host_cancels_and_rejects_lan_discovery() {
        assert!(start_discovery_scan());
        cancel_discovery_scan();
        let deadline = Instant::now() + Duration::from_secs(3);
        while lock(discover_slot())
            .as_ref()
            .is_some_and(|state| state.working)
        {
            assert!(Instant::now() < deadline, "cancelled LAN scan did not stop");
            thread::sleep(Duration::from_millis(10));
        }

        let server = start_server(Ipv4Addr::new(100, 64, 0, 1), true).unwrap();
        assert!(server.tailnet);
        assert!(server.discovery_thread.is_none());
        {
            let mut slot = lock(server_slot());
            assert!(
                slot.is_none(),
                "another test left the local room server running"
            );
            *slot = Some(server);
        }
        assert_eq!(unsafe { lan_discover_start() }, 0);
        unsafe { lan_host_stop() };
        assert!(lock(server_slot()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn local_address_prefers_wifi_over_vpn_route() {
        let wifi = Ipv4Addr::new(192, 168, 1, 42);
        let vpn = Ipv4Addr::new(10, 8, 0, 2);
        let selected = select_local_ipv4(
            vec![("wlan0".into(), wifi), ("tun0".into(), vpn)],
            Some(vpn),
        )
        .unwrap();
        assert_eq!(selected, wifi);
    }
    #[cfg(unix)]
    #[test]
    fn local_address_prefers_wifi_over_other_private_route() {
        let wifi = Ipv4Addr::new(192, 168, 1, 42);
        let other = Ipv4Addr::new(10, 0, 0, 7);
        let selected = select_local_ipv4(
            vec![("wlan0".into(), wifi), ("eth0".into(), other)],
            Some(other),
        )
        .unwrap();
        assert_eq!(selected, wifi);
    }

    #[test]
    fn host_advertisement_stays_assigned_and_suppresses_lost_interfaces() {
        let selected = Ipv4Addr::new(192, 168, 1, 42);
        let fallback = Ipv4Addr::new(192, 168, 1, 43);
        let current_interfaces = vec![("wlan0".into(), selected, true)];
        assert_eq!(
            live_host_address(
                "192.168.1.42:48100",
                48100,
                &current_interfaces,
                Some(fallback)
            ),
            Some("192.168.1.42:48100".into())
        );

        let changed_interfaces = vec![("wlan0".into(), fallback, true)];
        assert_eq!(
            live_host_address(
                "192.168.1.42:48100",
                48100,
                &changed_interfaces,
                Some(fallback)
            ),
            Some("192.168.1.43:48100".into())
        );
        assert_eq!(
            live_host_address("192.168.1.42:48100", 48100, &[], Some(selected)),
            None
        );
    }
}
