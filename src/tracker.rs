//! Folds successive socket snapshots into per-application totals.

use crate::collector::{Flow, Protocol};
use crate::geo::{is_public, Geo, Place};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DestKey {
    org: String,
    cc: String,
}

struct Dest {
    place: Place,
    sent: u64,
    received: u64,
    active: u32,
    connections: u32,
    ips: BTreeSet<IpAddr>,
    ports: BTreeSet<(u16, bool)>,
    last_active: Instant,
}

struct Socket {
    sent: u64,
    received: u64,
    seen_at: u64,
}

pub struct Tracker {
    geo: Geo,
    include_local: bool,
    byte_counts: bool,
    started: Instant,
    tick: u64,
    apps: BTreeMap<String, BTreeMap<DestKey, Dest>>,
    /// Processes currently holding a connection, per application.
    pids: BTreeMap<String, BTreeSet<u32>>,
    sockets: HashMap<u64, Socket>,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub version: &'static str,
    pub platform: &'static str,
    pub uptime_s: u64,
    pub byte_counts: bool,
    pub located: bool,
    pub apps: Vec<AppView>,
}

#[derive(Serialize)]
pub struct AppView {
    pub name: String,
    pub sent: u64,
    pub received: u64,
    pub active: u32,
    pub processes: usize,
    pub dests: Vec<DestView>,
}

#[derive(Serialize)]
pub struct DestView {
    #[serde(flatten)]
    pub place: Place,
    pub sent: u64,
    pub received: u64,
    pub active: u32,
    pub connections: u32,
    pub ips: usize,
    /// "443" for TCP, "443/udp" for UDP (QUIC, mostly).
    pub ports: Vec<String>,
    pub idle_ms: u64,
}

impl Tracker {
    pub fn new(geo: Geo, include_local: bool, byte_counts: bool) -> Self {
        Self {
            geo,
            include_local,
            byte_counts,
            started: Instant::now(),
            tick: 0,
            apps: BTreeMap::new(),
            pids: BTreeMap::new(),
            sockets: HashMap::new(),
        }
    }

    pub fn update(&mut self, flows: &[Flow], now: Instant) {
        self.tick += 1;
        self.pids.clear();
        for dests in self.apps.values_mut() {
            for dest in dests.values_mut() {
                dest.active = 0;
            }
        }

        for flow in flows {
            if !self.include_local && !is_public(flow.remote) {
                continue;
            }
            let place = self.geo.locate(flow.remote);
            let key = DestKey {
                org: place.org.clone(),
                cc: place.cc.clone(),
            };
            let dest = self
                .apps
                .entry(flow.app.clone())
                .or_default()
                .entry(key)
                .or_insert_with(|| Dest {
                    place,
                    sent: 0,
                    received: 0,
                    active: 0,
                    connections: 0,
                    ips: BTreeSet::new(),
                    ports: BTreeSet::new(),
                    last_active: now,
                });

            // Sockets that were already open when watching began carry bytes sent
            // earlier: take their counters as the baseline instead of counting them.
            let preexisting = self.tick == 1;
            let socket = self.sockets.entry(flow.key).or_insert_with(|| {
                dest.connections += 1;
                dest.last_active = now;
                Socket {
                    sent: if preexisting {
                        flow.bytes_sent.unwrap_or(0)
                    } else {
                        0
                    },
                    received: if preexisting {
                        flow.bytes_received.unwrap_or(0)
                    } else {
                        0
                    },
                    seen_at: 0,
                }
            });
            dest.ips.insert(flow.remote);
            dest.ports
                .insert((flow.remote_port, flow.protocol == Protocol::Udp));
            if let Some(pid) = flow.pid {
                self.pids.entry(flow.app.clone()).or_default().insert(pid);
            }
            dest.active += 1;
            socket.seen_at = self.tick;

            // Counters are cumulative per socket; only the growth since the last poll is new traffic.
            if let Some(sent) = flow.bytes_sent {
                let delta = sent.saturating_sub(socket.sent);
                if delta > 0 {
                    dest.sent += delta;
                    dest.last_active = now;
                }
                socket.sent = sent;
            }
            if let Some(received) = flow.bytes_received {
                let delta = received.saturating_sub(socket.received);
                if delta > 0 {
                    dest.received += delta;
                    dest.last_active = now;
                }
                socket.received = received;
            }
        }

        let tick = self.tick;
        self.sockets.retain(|_, s| s.seen_at == tick);
    }

    pub fn snapshot(&self, now: Instant) -> Snapshot {
        let mut apps: Vec<AppView> = self
            .apps
            .iter()
            .map(|(name, dests)| {
                let mut views: Vec<DestView> = dests
                    .values()
                    .map(|d| DestView {
                        place: d.place.clone(),
                        sent: d.sent,
                        received: d.received,
                        active: d.active,
                        connections: d.connections,
                        ips: d.ips.len(),
                        ports: d
                            .ports
                            .iter()
                            .take(8)
                            .map(|(port, udp)| {
                                if *udp {
                                    format!("{port}/udp")
                                } else {
                                    port.to_string()
                                }
                            })
                            .collect(),
                        idle_ms: now.saturating_duration_since(d.last_active).as_millis() as u64,
                    })
                    .collect();
                views.sort_by_key(|d| std::cmp::Reverse((d.sent, d.connections)));
                AppView {
                    name: name.clone(),
                    sent: views.iter().map(|d| d.sent).sum(),
                    received: views.iter().map(|d| d.received).sum(),
                    active: views.iter().map(|d| d.active).sum(),
                    processes: self.pids.get(name).map_or(0, BTreeSet::len),
                    dests: views,
                }
            })
            .collect();
        apps.sort_by(|a, b| (b.sent, b.active, &a.name).cmp(&(a.sent, a.active, &b.name)));

        Snapshot {
            version: env!("CARGO_PKG_VERSION"),
            platform: std::env::consts::OS,
            uptime_s: now.saturating_duration_since(self.started).as_secs(),
            byte_counts: self.byte_counts,
            located: self.geo.has_databases(),
            apps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn flow(key: u64, app: &str, remote: &str, sent: u64) -> Flow {
        Flow {
            key,
            pid: Some(1),
            app: app.to_string(),
            remote: remote.parse().unwrap(),
            remote_port: 443,
            protocol: Protocol::Tcp,
            bytes_sent: Some(sent),
            bytes_received: Some(0),
        }
    }

    /// A tracker that has already been watching for one poll.
    fn tracker(include_local: bool) -> Tracker {
        // No databases in this directory: every public address is one "Unknown network".
        let mut t = Tracker::new(Geo::open(Path::new("/nonexistent")), include_local, true);
        t.update(&[], Instant::now());
        t
    }

    #[test]
    fn ignores_bytes_sent_before_watching_began() {
        let mut t = Tracker::new(Geo::open(Path::new("/nonexistent")), false, true);
        let now = Instant::now();
        t.update(&[flow(1, "backup", "8.8.8.8", 5_000_000)], now);
        assert_eq!(t.snapshot(now).apps[0].sent, 0);
        t.update(&[flow(1, "backup", "8.8.8.8", 5_000_200)], now);
        assert_eq!(t.snapshot(now).apps[0].sent, 200);
    }

    #[test]
    fn counts_only_the_growth_between_polls() {
        let mut t = tracker(false);
        let now = Instant::now();
        t.update(&[flow(1, "curl", "8.8.8.8", 1_000)], now);
        t.update(&[flow(1, "curl", "8.8.8.8", 1_500)], now);
        t.update(&[flow(1, "curl", "8.8.8.8", 1_500)], now);
        let snap = t.snapshot(now);
        assert_eq!(snap.apps[0].sent, 1_500);
        assert_eq!(snap.apps[0].dests[0].connections, 1);
    }

    #[test]
    fn keeps_totals_after_a_connection_closes() {
        let mut t = tracker(false);
        let now = Instant::now();
        t.update(&[flow(1, "curl", "8.8.8.8", 700)], now);
        t.update(&[], now);
        let snap = t.snapshot(now);
        assert_eq!(snap.apps[0].sent, 700);
        assert_eq!(snap.apps[0].active, 0);

        // A new socket reusing nothing from the old one adds to the same destination.
        t.update(&[flow(2, "curl", "8.8.4.4", 300)], now);
        let snap = t.snapshot(now);
        assert_eq!(snap.apps[0].sent, 1_000);
        assert_eq!(snap.apps[0].dests[0].connections, 2);
        assert_eq!(snap.apps[0].dests[0].ips, 2);
    }

    #[test]
    fn hides_local_traffic_unless_asked() {
        let now = Instant::now();
        let flows = [
            flow(1, "ssh", "192.168.1.20", 10),
            flow(2, "node", "127.0.0.1", 10),
        ];

        let mut hidden = tracker(false);
        hidden.update(&flows, now);
        assert!(hidden.snapshot(now).apps.is_empty());

        let mut shown = tracker(true);
        shown.update(&flows, now);
        let orgs: Vec<String> = shown
            .snapshot(now)
            .apps
            .iter()
            .map(|a| a.dests[0].place.org.clone())
            .collect();
        assert_eq!(orgs, ["This machine", "Local network"]);
    }

    #[test]
    fn ranks_apps_by_bytes_sent() {
        let mut t = tracker(false);
        let now = Instant::now();
        t.update(
            &[
                flow(1, "quiet", "8.8.8.8", 10),
                flow(2, "chatty", "1.1.1.1", 9_000),
            ],
            now,
        );
        let names: Vec<String> = t.snapshot(now).apps.into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["chatty", "quiet"]);
    }
}
