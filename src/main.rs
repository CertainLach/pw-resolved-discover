#![feature(ip)]

use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    ptr::null_mut,
    result,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use dbus::blocking::SyncConnection;
use derivative::Derivative;
use libc::{fclose, fprintf, free, open_memstream};
use pipewire::{
    properties,
    spa::{ReadableDict, WritableDict},
    Context,
};
use pipewire_sys::pw_impl_module;
use real_c_string::real_c_string;

use crate::{
    resolve1::OrgFreedesktopResolve1Manager,
    rr::{parse_name, parse_rr},
};
mod resolve1;
mod rr;

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("dbus: {0}")]
    Dbus(#[from] dbus::Error),
    #[error("parsing: {0}")]
    Nom(String),
    #[error("pipewire: {0}")]
    Pipewire(#[from] pipewire::Error),
    #[error("spa: {0}")]
    Spa(#[from] pipewire::spa::Error),
}
impl From<nom::Err<nom::error::Error<&[u8]>>> for Error {
    fn from(value: nom::Err<nom::error::Error<&[u8]>>) -> Self {
        Self::Nom(value.to_string())
    }
}
type Result<T, E = Error> = result::Result<T, E>;

const DEST: &str = "org.freedesktop.resolve1";
const PATH: &str = "/org/freedesktop/resolve1";
const RECORD: &str = "_raop._tcp.local";

const IFINDEX_ANY: i32 = 0;

const CLASS_IN: u16 = 1;
const TYPE_PTR: u16 = 12;

const MDNS_V4: u64 = 8;
const MDNS_V6: u64 = 16;

const AF_INET4: i32 = 2;
const AF_INET6: i32 = 10;

#[derive(Hash, PartialEq, Eq, Debug)]
struct TunnelKey {
    hostname: String,
    socket: SocketAddr,
}
struct Tunnel {
    module: *mut pw_impl_module,
}

struct Discovered {
    hostname: String,
    socket: SocketAddr,
    records: Vec<String>,
}

macro_rules! try_continue {
    ($v:expr) => {
        match $v {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                continue;
            }
        }
    };
}

#[derive(Debug, Clone, Derivative)]
#[derivative(PartialEq, Eq, PartialOrd, Ord)]
struct ResolvedHost {
    ifindex: i32,
    name: String,
    domain: String,
    #[derivative(PartialEq = "ignore", PartialOrd = "ignore", Ord = "ignore")]
    retries: u32,
}

fn found_mdns() {
    let connection = SyncConnection::new_system().expect("system connection failed");
    std::thread::spawn(move || {
        let proxy = connection.with_proxy(DEST, PATH, Duration::from_millis(2000));
        let mut resolved = BTreeSet::new();
        loop {
            let mut resolved_this_time = BTreeSet::new();
            let (records, _flags) = try_continue!(proxy.resolve_record(
                IFINDEX_ANY,
                RECORD,
                CLASS_IN,
                TYPE_PTR,
                MDNS_V4 | MDNS_V6
            ));
            for record in records {
                let (ifindex, class, type_, data) = record;
                if class != CLASS_IN || type_ != TYPE_PTR {
                    eprintln!("unexpected class/type record");
                    continue;
                }
                let (_rest, rr) = try_continue!(parse_rr(&data));
                if rr.class != CLASS_IN || rr.type_ != TYPE_PTR {
                    eprintln!("unexpected class/type rr");
                    continue;
                }
                let (_rest, domain) = try_continue!(parse_name(&rr.rdata));
                resolved_this_time.insert(ResolvedHost {
                    ifindex,
                    name: rr.name,
                    domain,
                    retries: 8,
                });
            }
            let mut readd = Vec::new();
            for removed in resolved.difference(&resolved_this_time) {
                if removed.retries == 0 {
                    eprintln!("removed host: {removed:?}")
                } else {
                    // Give host some time before finally removing it
                    // in case of mdns cache flushes et cetera
                    let mut removed = removed.clone();
                    removed.retries -= 1;
                    readd.push(removed);
                }
            }
            resolved_this_time.extend(readd);
            for added in resolved_this_time.difference(&resolved) {
                eprintln!("added host: {added:?}")
            }
            resolved = resolved_this_time;
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

fn resolved_mdns() -> Receiver<Discovered> {
    found_mdns();
    let (tx, rx) = mpsc::channel();
    let connection = SyncConnection::new_system().expect("system connection failed");
    std::thread::spawn(move || {
        // FIXME: Ipv6 doesn't work, RAOP sink doesn't supports link-local addresses
        // TODO: Should be raop.ip.scope_id be added to pipewire module?
        let v4 = true;
        let proxy = connection.with_proxy(DEST, PATH, Duration::from_millis(2000));
        loop {
            eprintln!("scanning, ipv4 = {v4}");
            let (records, flags) = try_continue!(proxy.resolve_record(
                IFINDEX_ANY,
                RECORD,
                CLASS_IN,
                TYPE_PTR,
                MDNS_V4 // | MDNS_V6
            ));
            // v4 = !v4;
            for record in records {
                let (_ifindex, _class, type_, data) = record;
                let (_rest, rr) = try_continue!(parse_rr(&data));
                if type_ != TYPE_PTR || rr.type_ != TYPE_PTR {
                    eprintln!("received non-ptr record on ptr request");
                    continue;
                }
                let (_rest, domain) = try_continue!(parse_name(&rr.rdata));
                let (srvs, records, _name, _service, _domain, _idk) = try_continue!(proxy
                    .resolve_service(
                        IFINDEX_ANY,
                        "",
                        "",
                        &domain,
                        if v4 { AF_INET4 } else { AF_INET6 },
                        0,
                    ));

                let records: Vec<_> = records
                    .into_iter()
                    .map(|r| String::from_utf8_lossy(&r).to_string())
                    .collect();

                for srv in srvs {
                    let (priority, weight, port, hostname, ips, domain) = srv;
                    for ip in ips {
                        let (ifindex, af, address) = ip;
                        let socket: SocketAddr = if af == AF_INET6 && address.len() == 16 {
                            let mut addr = [0; 16];
                            addr.copy_from_slice(&address);
                            let addr = Ipv6Addr::from(addr);
                            SocketAddrV6::new(
                                addr,
                                port,
                                0,
                                if addr.is_unicast_link_local() {
                                    ifindex as u32
                                } else {
                                    0
                                },
                            )
                            .into()
                            // SocketAddrV6::new(, port)
                        } else if af == AF_INET4 && address.len() == 4 {
                            let mut addr = [0; 4];
                            addr.copy_from_slice(&address);
                            SocketAddrV4::new(Ipv4Addr::from(addr), port).into()
                        } else {
                            eprintln!("unknown address family: {af} {address:?}");
                            continue;
                        };

                        if tx
                            .send(Discovered {
                                hostname: hostname.clone(),
                                socket,
                                records: records.clone(),
                            })
                            .is_err()
                        {
                            eprintln!("receiver is dead");
                            return;
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
    rx
}

fn main() -> Result<()> {
    let pw = pipewire::MainLoop::new()?;
    let context = Context::new(&pw)?;

    let mut tunnels = RefCell::new(<HashMap<TunnelKey, Tunnel>>::new());

    let rx = resolved_mdns();

    let timer = pw.add_timer(move |_t| {
        let _measurer = Measurer(Instant::now());
        let Ok(msg) = rx.recv_timeout(Duration::from_millis(0)) else {
            return;
        };
        let key = TunnelKey {
            hostname: msg.hostname.clone(),
            socket: msg.socket,
        };
        if tunnels.borrow().contains_key(&key) {
            return;
        }
        let readable_name = msg
            .records
            .iter()
            .find_map(|r| r.strip_prefix("am="))
            .map(|v| v.to_owned())
            .unwrap_or_else(|| "<unnamed>".to_owned());
        let address = msg.socket.ip();
        let port = msg.socket.port();
        let mut prop = properties! {
            "raop.ip" => address.to_string(),
            "raop.ip.version" => match address {
                IpAddr::V4(_) => "4",
                IpAddr::V6(_) => "6",
            },
            "raop.port" => port.to_string(),
            "raop.name" => {
                let mut name = format!("{readable_name}");
                if address.is_ipv4() {
                    name.push_str(" (IPv4)");
                }
                name
            },
            "raop.hostname" => msg.hostname.as_str(),
        };
        for record in &msg.records {
            // comma-separated list contains
            fn clc(l: &str, v: &str) -> bool {
                l.split(',').any(|i| i == v)
            }
            if let Some(tp) = record.strip_prefix("tp=") {
                if tp.split(",").any(|v| v == "UDP") {
                    prop.insert("raop.transport", "udp")
                } else if tp.split(",").any(|v| v == "TCP") {
                    prop.insert("raop.transport", "tcp")
                } else {
                    eprintln!("unknown transport: {tp}");
                }
            } else if let Some(et) = record.strip_prefix("et=") {
                if et.split(',').any(|v| v == "1") {
                    prop.insert("raop.encryption.type", "RSA")
                } else if et.split(',').any(|v| v == "4") {
                    prop.insert("raop.encryption.type", "auth_setup")
                } else {
                    eprintln!("unknown encryption type: {et}");
                    prop.insert("raop.encryption.type", "none")
                }
            } else if let Some(cn) = record.strip_prefix("cn=") {
                prop.insert(
                    "raop.audio.codec",
                    if clc(cn, "3") {
                        "AAC-ELD"
                    } else if clc(cn, "2") {
                        "AAC"
                    } else if clc(cn, "1") {
                        "ALAC"
                    } else if clc(cn, "0") {
                        "PCM"
                    } else {
                        eprintln!("unknown codec: {cn}");
                        continue;
                    },
                )
            }
        }
        // prop.insert(key, value);
        let mut ptr = null_mut();
        let mut sizeloc = 0;

        let module = unsafe {
            let stream = open_memstream(&mut ptr, &mut sizeloc);
            if stream.is_null() {
                panic!("memstream failed");
            };
            fprintf(stream, real_c_string!("{"));
            pipewire_sys::pw_properties_serialize_dict(stream.cast(), prop.get_dict_ptr(), 0);
            fprintf(stream, real_c_string!("}"));
            fclose(stream);

            let module = pipewire_sys::pw_context_load_module(
                context.as_ptr(),
                real_c_string!("libpipewire-module-raop-sink"),
                ptr,
                null_mut(),
            );
            free(ptr.cast());

            module
        };
        eprintln!("discovered new tunnel: {key:?}");
        tunnels.borrow_mut().insert(key, Tunnel { module });
    });

    timer.update_timer(Some(Duration::from_millis(1)), Some(Duration::from_secs(3)));

    pw.run();
    Ok(())
}

struct Measurer(Instant);
impl Drop for Measurer {
    fn drop(&mut self) {
        let elapsed = self.0.elapsed();
        if elapsed < Duration::from_millis(1) {
            return;
        }
        eprintln!("took {elapsed:?}")
    }
}
