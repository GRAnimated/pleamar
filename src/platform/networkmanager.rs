//! The network, through NetworkManager on the system bus: what it says and
//! what it can be asked. It is told of changes (its signals), not polled; a
//! system without NetworkManager falls back to iwd (`iwd`), which answers the
//! same, and with neither to what the kernel says (`system::network`), which
//! can only be read.
//!
//! `{ online, kind = "wifi" | "wired" | "none", name, strength, wifi, error,
//!    networks = { { ssid, strength, secure, known, active }, … } }`: the
//! connection in use, whether the wifi radio is on, and the networks in range,
//! the strongest first (one per name), with whether one has been used before.
//!
//! Commands (`network.*`): `wifi(on)`, `scan()`, `connect(ssid[, password])`,
//! `disconnect()`, `forget(ssid)`.

use super::SysValue;
use std::collections::HashMap;
use std::time::Duration;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::MatchRule;

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const ACCESS_POINT: &str = "org.freedesktop.NetworkManager.AccessPoint";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const SAVED: &str = "org.freedesktop.NetworkManager.Settings.Connection";
/// `NM_DEVICE_TYPE_WIFI`.
const WIFI_DEVICE: u32 = 2;

fn proxy<'a>(c: &Connection, path: impl Into<String>, iface: &'static str) -> Option<Proxy<'a>> {
    Proxy::new(c, NM, path.into(), iface).ok()
}

/// Whether NetworkManager is there to be asked.
pub fn available() -> bool {
    Connection::system().is_ok_and(|c| present(&c))
}

fn present(c: &Connection) -> bool {
    proxy(c, NM_PATH, NM).is_some_and(|p| p.get_property::<u32>("State").is_ok())
}

fn wifi_devices(c: &Connection) -> Vec<OwnedObjectPath> {
    let Some(nm) = proxy(c, NM_PATH, NM) else { return Vec::new() };
    let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).unwrap_or_default();
    devices
        .into_iter()
        .filter(|d| proxy(c, d.as_str(), DEVICE).and_then(|p| p.get_property::<u32>("DeviceType").ok()) == Some(WIFI_DEVICE))
        .collect()
}

/// The saved connections: path, name and, for a wifi one, its network's name.
fn saved(c: &Connection) -> Vec<(OwnedObjectPath, String, Option<String>)> {
    let Some(settings) = proxy(c, SETTINGS_PATH, SETTINGS) else { return Vec::new() };
    let paths: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).unwrap_or_default();
    paths
        .into_iter()
        .filter_map(|p| {
            let s: HashMap<String, HashMap<String, OwnedValue>> = proxy(c, p.as_str(), SAVED)?.call("GetSettings", &()).ok()?;
            let id = s.get("connection").and_then(|m| m.get("id")).and_then(|v| <&str>::try_from(v).ok()).unwrap_or_default().to_owned();
            let ssid = s.get("802-11-wireless").and_then(|m| m.get("ssid")).and_then(|v| <Vec<u8>>::try_from(v.clone()).ok()).map(|b| String::from_utf8_lossy(&b).into_owned());
            Some((p, id, ssid))
        })
        .collect()
}

struct Network {
    ssid: String,
    strength: f64,
    secure: bool,
    active: bool,
}

/// The networks in range: one per name, the strongest of its access points.
fn networks(c: &Connection) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    for dev in wifi_devices(c) {
        let Some(w) = proxy(c, dev.as_str(), WIRELESS) else { continue };
        let active: Option<OwnedObjectPath> = w.get_property("ActiveAccessPoint").ok();
        let points: Vec<OwnedObjectPath> = w.call("GetAllAccessPoints", &()).unwrap_or_default();
        for ap in points {
            let Some(p) = proxy(c, ap.as_str(), ACCESS_POINT) else { continue };
            let ssid = p.get_property::<Vec<u8>>("Ssid").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            if ssid.is_empty() {
                continue;
            }
            let strength = p.get_property::<u8>("Strength").unwrap_or(0) as f64 / 100.0;
            let flags = |k: &str| p.get_property::<u32>(k).unwrap_or(0);
            let secure = flags("WpaFlags") != 0 || flags("RsnFlags") != 0 || flags("Flags") & 1 != 0;
            let is_active = active.as_ref().is_some_and(|a| a == &ap);
            match out.iter_mut().find(|n| n.ssid == ssid) {
                Some(n) => {
                    n.strength = n.strength.max(strength);
                    n.active |= is_active;
                }
                None => out.push(Network { ssid, strength, secure, active: is_active }),
            }
        }
    }
    out.sort_by(|a, b| b.active.cmp(&a.active).then(b.strength.total_cmp(&a.strength)));
    out.truncate(24);
    out
}

fn now(c: &Connection) -> SysValue {
    let text = |s: &str| SysValue::Text(s.to_owned());
    let nm = proxy(c, NM_PATH, NM);
    let get_u32 = |k: &str| nm.as_ref().and_then(|p| p.get_property::<u32>(k).ok()).unwrap_or(0);
    // NM_STATE_CONNECTED_SITE (60) or GLOBAL (70): something is connected.
    let online = get_u32("State") >= 60;
    let wifi_on = nm.as_ref().and_then(|p| p.get_property::<bool>("WirelessEnabled").ok()).unwrap_or(false);
    let primary: Option<OwnedObjectPath> = nm.as_ref().and_then(|p| p.get_property("PrimaryConnection").ok()).filter(|p: &OwnedObjectPath| p.as_str() != "/");
    let (mut kind, mut name) = ("none", String::new());
    if let Some(a) = primary.as_ref().and_then(|p| proxy(c, p.as_str(), ACTIVE)) {
        let t = a.get_property::<String>("Type").unwrap_or_default();
        kind = if t == "802-11-wireless" { "wifi" } else { "wired" };
        name = a.get_property::<String>("Id").unwrap_or_default();
    }
    let networks = if wifi_on { networks(c) } else { Vec::new() };
    let known: Vec<String> = saved(c).into_iter().filter_map(|(_, _, s)| s).collect();
    let mut strength = if kind == "wired" { 1.0 } else { 0.0 };
    if let Some(n) = networks.iter().find(|n| n.active) {
        // In tenths: the signal jitters nonstop and is not worth telling every change.
        strength = (n.strength * 10.0).round() / 10.0;
        name = n.ssid.clone();
    }
    SysValue::Map(vec![
        ("online".into(), SysValue::Bool(online && kind != "none")),
        ("kind".into(), text(kind)),
        ("name".into(), SysValue::Text(name)),
        ("strength".into(), SysValue::Num(strength)),
        ("wifi".into(), SysValue::Bool(wifi_on)),
        // A connection that does not get in is not told here yet: the network never turns `active`.
        ("error".into(), text("none")),
        (
            "networks".into(),
            SysValue::List(
                networks
                    .iter()
                    .map(|n| {
                        SysValue::Map(vec![
                            ("ssid".into(), SysValue::Text(n.ssid.clone())),
                            ("strength".into(), SysValue::Num((n.strength * 10.0).round() / 10.0)),
                            ("secure".into(), SysValue::Bool(n.secure)),
                            ("known".into(), SysValue::Bool(known.contains(&n.ssid))),
                            ("active".into(), SysValue::Bool(n.active)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

/// Watches NetworkManager, if it is there. `false` if it is not: the caller
/// falls back to the kernel.
pub fn service(dispatch: Box<dyn Fn(SysValue) + Send>) -> bool {
    let Ok(c) = Connection::system() else { return false };
    if !present(&c) {
        return false;
    }
    std::thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let mut last = String::new();
            let mut report = |c: &Connection| {
                let v = now(c);
                let fingerprint = format!("{v:?}");
                if fingerprint != last {
                    last = fingerprint;
                    dispatch(v);
                }
            };
            report(&c);
            // Anything NetworkManager says: a connection, a device, an access point.
            let rule = MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(NM).map(|b| b.build());
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            if let Ok(rule) = rule {
                let (c, tx) = (c.clone(), tx.clone());
                std::thread::spawn(move || {
                    let Ok(messages) = MessageIterator::for_match_rule(rule, &c, Some(64)) else { return };
                    for _ in messages {
                        if tx.send(()).is_err() {
                            break;
                        }
                    }
                });
            }
            loop {
                // A scan or a connection is a burst of signals: told once, after it.
                match rx.recv_timeout(Duration::from_secs(20)) {
                    Ok(()) => {
                        std::thread::sleep(Duration::from_millis(250));
                        while rx.try_recv().is_ok() {}
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                report(&c);
            }
        })
        .is_ok()
}

pub fn command(what: &str, args: &[SysValue]) -> Result<(), String> {
    let c = Connection::system().map_err(|e| e.to_string())?;
    if !present(&c) {
        return Err("neither NetworkManager nor iwd is running: the network can only be read".into());
    }
    let nm = proxy(&c, NM_PATH, NM).ok_or("NetworkManager does not answer")?;
    let err = |e: zbus::Error| e.to_string();
    match (what, args) {
        ("network.wifi", [SysValue::Bool(on)]) => nm.set_property("WirelessEnabled", *on).map_err(|e| e.to_string()),
        ("network.scan", []) => {
            for d in wifi_devices(&c) {
                if let Some(w) = proxy(&c, d.as_str(), WIRELESS) {
                    let options: HashMap<&str, Value> = HashMap::new();
                    let _ = w.call_method("RequestScan", &(options,));
                }
            }
            Ok(())
        }
        ("network.connect", [SysValue::Text(ssid)]) | ("network.connect", [SysValue::Text(ssid), _]) => {
            let password = match args.get(1) {
                Some(SysValue::Text(p)) if !p.is_empty() => Some(p.clone()),
                _ => None,
            };
            let root = OwnedObjectPath::try_from("/").map_err(|e| e.to_string())?;
            // Used before (and no new password): that one, as it was saved.
            if let (Some((path, _, _)), None) = (saved(&c).into_iter().find(|(_, _, s)| s.as_deref() == Some(ssid.as_str())), &password) {
                return nm.call_method("ActivateConnection", &(path, root.clone(), root)).map(|_| ()).map_err(err);
            }
            let device = wifi_devices(&c).into_iter().next().ok_or("there is no wifi device")?;
            let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
            settings.insert("connection", HashMap::from([("type", Value::from("802-11-wireless")), ("id", Value::from(ssid.as_str()))]));
            settings.insert("802-11-wireless", HashMap::from([("ssid", Value::from(ssid.as_bytes().to_vec()))]));
            if let Some(p) = password {
                settings.insert("802-11-wireless-security", HashMap::from([("key-mgmt", Value::from("wpa-psk")), ("psk", Value::from(p))]));
            }
            nm.call_method("AddAndActivateConnection", &(settings, device, root)).map(|_| ()).map_err(err)
        }
        ("network.disconnect", []) => {
            let primary: OwnedObjectPath = nm.get_property("PrimaryConnection").map_err(|e| e.to_string())?;
            if primary.as_str() == "/" {
                return Ok(());
            }
            nm.call_method("DeactivateConnection", &(primary,)).map(|_| ()).map_err(err)
        }
        ("network.forget", [SysValue::Text(ssid)]) => {
            let (path, _, _) = saved(&c).into_iter().find(|(_, _, s)| s.as_deref() == Some(ssid.as_str())).ok_or("that network was never saved")?;
            proxy(&c, path.as_str(), SAVED).ok_or("NetworkManager does not answer")?.call_method("Delete", &()).map(|_| ()).map_err(err)
        }
        _ => Err(format!("'{what}' is not asked like that: network.wifi(true|false), network.scan(), network.connect(ssid[, password]), network.disconnect(), network.forget(ssid)")),
    }
}
