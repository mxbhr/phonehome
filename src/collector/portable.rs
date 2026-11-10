//! macOS and Windows backend: reads the OS socket table.
//!
//! These systems do not expose per-socket byte counters without elevated
//! privileges, so flows carry no byte counts here.

use super::{Collector, Flow, Protocol};
use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use sysinfo::{Pid, ProcessesToUpdate, System};

pub struct TableCollector {
    system: System,
}

impl TableCollector {
    pub fn new() -> Self {
        Self {
            system: System::new(),
        }
    }
}

impl Collector for TableCollector {
    fn poll(&mut self) -> anyhow::Result<Vec<Flow>> {
        let sockets = get_sockets_info(
            AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
            ProtocolFlags::TCP,
        )
        .map_err(|e| anyhow::anyhow!("reading the socket table failed: {e}"))?;
        self.system.refresh_processes(ProcessesToUpdate::All, true);

        let mut flows = Vec::new();
        for socket in sockets {
            let ProtocolSocketInfo::Tcp(tcp) = &socket.protocol_socket_info else {
                continue;
            };
            if matches!(tcp.state, TcpState::Listen | TcpState::Closed) || tcp.remote_port == 0 {
                continue;
            }
            let pid = socket.associated_pids.first().copied();
            let app = pid
                .and_then(|pid| self.system.process(Pid::from_u32(pid)))
                .map(|p| {
                    p.name()
                        .to_string_lossy()
                        .trim_end_matches(".exe")
                        .to_string()
                })
                .unwrap_or_else(|| "unknown".to_string());

            let mut hasher = DefaultHasher::new();
            (pid, tcp.local_port, tcp.remote_addr, tcp.remote_port).hash(&mut hasher);

            flows.push(Flow {
                key: hasher.finish(),
                pid,
                app,
                remote: tcp.remote_addr,
                remote_port: tcp.remote_port,
                protocol: Protocol::Tcp,
                bytes_sent: None,
                bytes_received: None,
            });
        }
        Ok(flows)
    }

    fn byte_counts(&self) -> bool {
        false
    }
}
