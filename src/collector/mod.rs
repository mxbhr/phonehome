//! Lists the sockets of this machine and attributes each one to a process.
//!
//! Linux talks to the kernel directly over netlink (`sock_diag`), which also
//! exposes per-socket byte counters. Other systems read the OS socket table.

use std::net::IpAddr;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod portable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// One open socket with a remote peer, as seen at poll time.
#[derive(Debug, Clone)]
pub struct Flow {
    /// Stable identity of the socket for as long as it stays open.
    pub key: u64,
    pub pid: Option<u32>,
    pub app: String,
    pub remote: IpAddr,
    pub remote_port: u16,
    pub protocol: Protocol,
    /// Cumulative payload bytes since the socket was opened, when the OS exposes them.
    pub bytes_sent: Option<u64>,
    pub bytes_received: Option<u64>,
}

pub trait Collector: Send {
    fn poll(&mut self) -> anyhow::Result<Vec<Flow>>;
    /// Whether this platform reports byte counters.
    fn byte_counts(&self) -> bool;
}

pub fn new() -> anyhow::Result<Box<dyn Collector>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::NetlinkCollector::new()?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(Box::new(portable::TableCollector::new()))
    }
}
