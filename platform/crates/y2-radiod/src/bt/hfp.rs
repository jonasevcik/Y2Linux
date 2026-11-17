//! The Hands-Free profile, as an audio gateway that only listens: headsets send their battery level
//! over a Hands-Free link right after they connect (a Mac, a phone, a PC all answer that link), and
//! the player had no gateway to answer, so the level never arrived. BlueZ opens the link and hands
//! its file descriptor to this profile; a thread per link (`Worker`) speaks the AT conversation
//! (`hfp_at`) and reports the level. No call and no sound ever starts here (the audio stays on
//! A2DP), and nothing a link does can reach the controller's connect logic: its thread talks to the
//! controller through one channel, and a link that stops or misbehaves is only closed.
// SPDX-License-Identifier: GPL-2.0-only
use super::{
    bus::Kind,
    hfp_at::{Reading, Session},
    Controller,
};
use crate::{desired::valid_address, wake::Waker};
use dbus::{
    arg::{PropMap, RefArg, Variant},
    Message,
};
use std::{
    collections::BTreeMap,
    io,
    ops::ControlFlow,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use y2p_core::sys;

pub const PATH: &str = "/org/y2/hfp";
/// Hands-Free Audio Gateway (the role the player plays toward a headset).
const UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";
/// Links at once: a headset has one; a few more cover one that reconnects before the old one ended.
const MAX_LINKS: usize = 4;
/// A link's thread only reads, answers and reports.
const STACK: usize = 64 * 1024;
/// How long a thread waits for bytes before it looks at its stop flag.
const LISTEN: Duration = Duration::from_secs(1);
/// A device BlueZ no longer shows as connected ends its link only after this, so that a link that
/// arrives before the observation saw the connection is not cut.
const GRACE: Duration = Duration::from_secs(10);
/// A link that sends more lines than a headset ever does (200 in 10 s) is closed.
const FLOOD: u32 = 200;
const FLOOD_WINDOW: Duration = Duration::from_secs(10);

/// What a link thread tells the controller.
pub enum Event {
    Line {
        address: String,
        line: String,
    },
    Level {
        id: u64,
        address: String,
        reading: Reading,
    },
    Closed {
        id: u64,
        address: String,
        why: &'static str,
    },
}

struct Link {
    id: u64,
    stop: Arc<AtomicBool>,
    since: Instant,
}

/// The controller's side of every link.
pub struct Hfp {
    sender: Sender<Event>,
    events: Receiver<Event>,
    links: BTreeMap<String, Link>,
    /// The last level each linked headset reported (gone with its link).
    pub(super) battery: BTreeMap<String, Reading>,
    next_id: u64,
}

impl Default for Hfp {
    fn default() -> Hfp {
        let (sender, events) = channel();
        Hfp {
            sender,
            events,
            links: BTreeMap::new(),
            battery: BTreeMap::new(),
            next_id: 0,
        }
    }
}

/// BlueZ's device path ends in `dev_AA_BB_CC_DD_EE_FF`.
fn address_of(path: &str) -> Option<String> {
    let address = path.rsplit_once("/dev_")?.1.replace('_', ":");
    valid_address(&address).then_some(address)
}

fn options() -> PropMap {
    let mut options = PropMap::new();
    let mut put = |key: &str, value: Box<dyn RefArg>| {
        options.insert(key.to_owned(), Variant(value));
    };
    put("Name", Box::new("Headphone battery".to_owned()));
    // No authorization question for a link of a device that is already paired.
    put("RequireAuthorization", Box::new(false));
    // Hands-Free 1.7 (hands-free indicators) and no voice feature: nothing here carries a call.
    put("Version", Box::new(0x0107u16));
    put("Features", Box::new(0u16));
    options
}

impl Controller {
    /// Register the profile with the BlueZ that runs now (again after BlueZ restarted), unless the
    /// owner switched it off in `radiod.json`. The answer is not waited for: a BlueZ that does not
    /// answer must not hold the controller.
    pub(super) fn register_hfp(&mut self) {
        if !self.desired.policy.headphone_battery {
            return;
        }
        let Some(owner) = self.world.bluez.clone() else {
            return;
        };
        let Some(bus) = self.bus.as_mut() else {
            return;
        };
        if bus.profile_owner.as_deref() == Some(owner.as_str()) {
            return;
        }
        let Ok(path) = dbus::Path::new(PATH) else {
            return;
        };
        bus.profile_owner = Some(owner.clone());
        let sent = bus.call(
            (
                &owner,
                "/org/bluez",
                "org.bluez.ProfileManager1",
                "RegisterProfile",
            ),
            (path, UUID, options()),
            Kind::HfpProfile,
            Duration::from_secs(5),
        );
        if !sent {
            self.journal
                .event("hfp_unavailable", serde_json::json!({"error": "not sent"}));
        }
    }

    /// A call from BlueZ to the profile.
    pub(super) fn hfp_call(&mut self, message: Message) {
        let member = message.member().map(|m| m.to_string()).unwrap_or_default();
        let reply = match member.as_str() {
            "NewConnection" => self.hfp_connection(&message),
            "RequestDisconnection" => {
                let address = message
                    .read1::<dbus::Path>()
                    .ok()
                    .and_then(|path| address_of(&path));
                if let Some(link) = address.and_then(|a| self.hfp.links.get(&a)) {
                    link.stop.store(true, Ordering::Relaxed);
                }
                message.method_return()
            }
            "Release" => message.method_return(),
            _ => message.error(
                &"org.freedesktop.DBus.Error.UnknownMethod".into(),
                c"No such method",
            ),
        };
        if let Some(bus) = &self.bus {
            bus.send(reply);
        }
    }

    fn hfp_connection(&mut self, message: &Message) -> Message {
        let rejected = || message.error(&"org.bluez.Error.Rejected".into(), c"No link now");
        let Ok((path, fd, _)) = message.read3::<dbus::Path, OwnedFd, PropMap>() else {
            return message.error(
                &"org.freedesktop.DBus.Error.InvalidArgs".into(),
                c"Bad arguments",
            );
        };
        let Some(address) = address_of(&path) else {
            return rejected();
        };
        if !self.hfp.links.contains_key(&address) && self.hfp.links.len() >= MAX_LINKS {
            return rejected();
        }
        self.hfp.next_id += 1;
        let id = self.hfp.next_id;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            id,
            address: address.clone(),
            fd,
            session: Session::default(),
            stop: stop.clone(),
            events: self.hfp.sender.clone(),
            waker: self.wake.waker(),
            recent: (Instant::now(), 0),
        };
        let started = thread::Builder::new()
            .name("radiod-hfp".into())
            .stack_size(STACK)
            .spawn(move || worker.run());
        if started.is_err() {
            return rejected();
        }
        // A link that replaces another one of the same device: the old one ends, and its level
        // with it (the new link reports its own).
        if let Some(old) = self.hfp.links.insert(
            address.clone(),
            Link {
                id,
                stop,
                since: Instant::now(),
            },
        ) {
            old.stop.store(true, Ordering::Relaxed);
        }
        self.hfp.battery.remove(&address);
        self.journal.event(
            "hfp_link",
            serde_json::json!({"address": address, "state": "open"}),
        );
        message.method_return()
    }

    /// Take what the link threads reported, and end the links of devices that are gone.
    pub(super) fn drain_hfp(&mut self, now: Instant) {
        while let Ok(event) = self.hfp.events.try_recv() {
            match event {
                Event::Line { address, line } => self.journal.event(
                    "hfp_at",
                    serde_json::json!({"address": address, "line": line}),
                ),
                Event::Level {
                    id,
                    address,
                    reading,
                } => self.hfp_level(id, address, reading),
                Event::Closed { id, address, why } => self.hfp_closed(id, &address, why),
            }
        }
        let gone: Vec<String> = self
            .hfp
            .links
            .iter()
            .filter(|(address, link)| {
                now.duration_since(link.since) >= GRACE
                    && !self
                        .world
                        .devices
                        .get(*address)
                        .is_some_and(|dev| dev.connected)
            })
            .map(|(address, _)| address.clone())
            .collect();
        for address in gone {
            if let Some(link) = self.hfp.links.get(&address) {
                link.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    fn hfp_level(&mut self, id: u64, address: String, reading: Reading) {
        // A level from a link that was replaced is stale.
        if self
            .hfp
            .links
            .get(&address)
            .is_none_or(|link| link.id != id)
        {
            return;
        }
        if self.hfp.battery.insert(address.clone(), reading) != Some(reading) {
            self.dirty = true;
            self.journal.event(
                "headphone_battery",
                serde_json::json!({
                    "address": address, "percent": reading.percent,
                    "source": reading.source.name()
                }),
            );
        }
    }

    fn hfp_closed(&mut self, id: u64, address: &str, why: &str) {
        if self.hfp.links.get(address).is_none_or(|link| link.id != id) {
            return;
        }
        self.hfp.links.remove(address);
        self.hfp.battery.remove(address);
        self.dirty = true;
        self.journal.event(
            "hfp_link",
            serde_json::json!({"address": address, "state": "closed", "why": why}),
        );
    }
}

/// One link's thread.
struct Worker {
    id: u64,
    address: String,
    fd: OwnedFd,
    session: Session,
    stop: Arc<AtomicBool>,
    events: Sender<Event>,
    waker: Waker,
    /// Lines in the current flood window: when it began and how many.
    recent: (Instant, u32),
}

impl Worker {
    fn run(mut self) {
        let why = loop {
            if self.stop.load(Ordering::Relaxed) {
                break "stopped";
            }
            if let ControlFlow::Break(why) = self.pass() {
                break why;
            }
        };
        self.send(Event::Closed {
            id: self.id,
            address: self.address.clone(),
            why,
        });
    }

    /// Wait up to `LISTEN` for bytes, answer every line in them.
    fn pass(&mut self) -> ControlFlow<&'static str> {
        if !self.ready(libc::POLLIN) {
            return ControlFlow::Continue(());
        }
        let mut buffer = [0u8; 512];
        let count = match sys::read(self.fd.as_fd(), &mut buffer) {
            Ok(0) => return ControlFlow::Break("ended"),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return ControlFlow::Continue(())
            }
            Err(_) => return ControlFlow::Break("read_failed"),
        };
        for step in self.session.feed(&buffer[..count]) {
            if self.flooded() {
                return ControlFlow::Break("flood");
            }
            if !self.answer(&step.reply) {
                return ControlFlow::Break("write_failed");
            }
            self.send(Event::Line {
                address: self.address.clone(),
                line: step.line,
            });
            if let Some(reading) = step.reading {
                self.send(Event::Level {
                    id: self.id,
                    address: self.address.clone(),
                    reading,
                });
            }
        }
        ControlFlow::Continue(())
    }

    /// Whether the descriptor shows `events` (or an end or an error, which the read then reports).
    fn ready(&self, events: libc::c_short) -> bool {
        let mut fds = [libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events,
            revents: 0,
        }];
        sys::poll(&mut fds, LISTEN, sys::Rounding::Up).is_ok_and(|ready| ready > 0)
    }

    /// Write the answer whole; false when the headset does not take it within a second.
    fn answer(&mut self, mut reply: &[u8]) -> bool {
        while !reply.is_empty() {
            if !self.ready(libc::POLLOUT) {
                return false;
            }
            match sys::write(self.fd.as_fd(), reply) {
                Ok(0) | Err(_) => return false,
                Ok(count) => reply = &reply[count..],
            }
        }
        true
    }

    fn flooded(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.recent.0) >= FLOOD_WINDOW {
            self.recent = (now, 0);
        }
        self.recent.1 += 1;
        self.recent.1 > FLOOD
    }

    fn send(&self, event: Event) {
        let _ = self.events.send(event);
        self.waker.wake();
    }
}
