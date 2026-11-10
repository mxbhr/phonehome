//! Linux backend: `NETLINK_SOCK_DIAG` dumps, the interface behind `ss`.
//!
//! One request per (family, protocol) returns every socket with its inode and,
//! for TCP, a `tcp_info` attribute holding byte counters. Inodes are then
//! matched against `/proc/<pid>/fd` to find the owning process.

use super::{Collector, Flow, Protocol};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const NETLINK_SOCK_DIAG: libc::c_int = 4;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const INET_DIAG_INFO: u16 = 2;
const TCP_CLOSE: u32 = 7;
const TCP_LISTEN: u32 = 10;

const NLMSG_HDR_LEN: usize = 16;
const DIAG_REQ_LEN: usize = 56;
const DIAG_MSG_LEN: usize = 72;

/// Offsets inside `struct tcp_info` (include/uapi/linux/tcp.h).
const TCP_INFO_BYTES_ACKED: usize = 120;
const TCP_INFO_BYTES_RECEIVED: usize = 128;

#[derive(Debug)]
struct RawSocket {
    protocol: Protocol,
    inode: u32,
    cookie: u64,
    remote: IpAddr,
    remote_port: u16,
    bytes_sent: Option<u64>,
    bytes_received: Option<u64>,
}

pub struct NetlinkCollector {
    fd: OwnedFd,
    seq: u32,
    /// inode -> owning process; `None` when no readable process holds it.
    owners: HashMap<u32, Option<(u32, String)>>,
}

impl NetlinkCollector {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                NETLINK_SOCK_DIAG,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        // Never block the collector forever on a kernel that stops answering.
        let timeout = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &timeout as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        Ok(Self {
            fd,
            seq: 0,
            owners: HashMap::new(),
        })
    }

    fn dump(&mut self, family: u8, protocol: Protocol) -> io::Result<Vec<RawSocket>> {
        self.seq = self.seq.wrapping_add(1);
        let (ipproto, states) = match protocol {
            // Every state except the ones without a peer.
            Protocol::Tcp => (
                libc::IPPROTO_TCP as u8,
                !(1u32 << TCP_LISTEN | 1u32 << TCP_CLOSE),
            ),
            Protocol::Udp => (libc::IPPROTO_UDP as u8, u32::MAX),
        };

        let mut req = [0u8; NLMSG_HDR_LEN + DIAG_REQ_LEN];
        let total = req.len() as u32;
        req[0..4].copy_from_slice(&total.to_ne_bytes());
        req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
        req[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
        req[8..12].copy_from_slice(&self.seq.to_ne_bytes());
        req[16] = family;
        req[17] = ipproto;
        req[18] = 1 << (INET_DIAG_INFO - 1); // ask for tcp_info
        req[20..24].copy_from_slice(&states.to_ne_bytes());

        let sent = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                req.as_ptr() as *const libc::c_void,
                req.len(),
                0,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut sockets = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if parse_messages(&buf[..n as usize], protocol, &mut sockets)? {
                return Ok(sockets);
            }
        }
    }

    fn refresh_owners(&mut self, wanted: &HashSet<u32>) {
        let mut found: HashMap<u32, (u32, String)> = HashMap::new();
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<u32>().ok())
                else {
                    continue;
                };
                let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                    continue;
                };
                let mut name: Option<String> = None;
                for fd in fds.flatten() {
                    let Ok(target) = fs::read_link(fd.path()) else {
                        continue;
                    };
                    let Some(inode) = socket_inode(&target.to_string_lossy()) else {
                        continue;
                    };
                    if wanted.contains(&inode) {
                        let name = name.get_or_insert_with(|| process_name(pid)).clone();
                        found.entry(inode).or_insert((pid, name));
                    }
                }
            }
        }
        self.owners.retain(|inode, _| wanted.contains(inode));
        for inode in wanted {
            self.owners.insert(*inode, found.remove(inode));
        }
    }
}

impl Collector for NetlinkCollector {
    fn poll(&mut self) -> anyhow::Result<Vec<Flow>> {
        let mut raw = Vec::new();
        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            raw.extend(self.dump(family, Protocol::Tcp)?);
            // udp_diag may be missing from the kernel; TCP alone is still useful.
            if let Ok(udp) = self.dump(family, Protocol::Udp) {
                raw.extend(udp);
            }
        }
        raw.retain(|s| s.inode != 0 && s.remote_port != 0 && !s.remote.is_unspecified());

        let wanted: HashSet<u32> = raw.iter().map(|s| s.inode).collect();
        if wanted.iter().any(|inode| !self.owners.contains_key(inode)) {
            self.refresh_owners(&wanted);
        } else {
            self.owners.retain(|inode, _| wanted.contains(inode));
        }

        Ok(raw
            .into_iter()
            .map(|s| {
                let owner = self.owners.get(&s.inode).cloned().flatten();
                Flow {
                    key: s.cookie,
                    pid: owner.as_ref().map(|(pid, _)| *pid),
                    app: owner
                        .map(|(_, name)| name)
                        .unwrap_or_else(|| "unknown".to_string()),
                    remote: s.remote,
                    remote_port: s.remote_port,
                    protocol: s.protocol,
                    bytes_sent: s.bytes_sent,
                    bytes_received: s.bytes_received,
                }
            })
            .collect())
    }

    fn byte_counts(&self) -> bool {
        true
    }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn u16_ne(b: &[u8]) -> u16 {
    u16::from_ne_bytes([b[0], b[1]])
}

fn u32_ne(b: &[u8]) -> u32 {
    u32::from_ne_bytes([b[0], b[1], b[2], b[3]])
}

fn u64_ne(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_ne_bytes(a)
}

/// Parses one datagram of netlink messages. Returns `true` once the dump is complete.
fn parse_messages(
    mut data: &[u8],
    protocol: Protocol,
    out: &mut Vec<RawSocket>,
) -> io::Result<bool> {
    while data.len() >= NLMSG_HDR_LEN {
        let len = u32_ne(&data[0..4]) as usize;
        let kind = u16_ne(&data[4..6]);
        if len < NLMSG_HDR_LEN || len > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated netlink message",
            ));
        }
        let payload = &data[NLMSG_HDR_LEN..len];
        match kind {
            NLMSG_DONE => return Ok(true),
            NLMSG_ERROR => {
                let errno = if payload.len() >= 4 {
                    -(u32_ne(payload) as i32)
                } else {
                    libc::EIO
                };
                return Err(io::Error::from_raw_os_error(errno));
            }
            SOCK_DIAG_BY_FAMILY => {
                if let Some(socket) = parse_diag_msg(payload, protocol) {
                    out.push(socket);
                }
            }
            _ => {}
        }
        data = &data[align4(len).min(data.len())..];
    }
    Ok(false)
}

fn parse_diag_msg(p: &[u8], protocol: Protocol) -> Option<RawSocket> {
    if p.len() < DIAG_MSG_LEN {
        return None;
    }
    let family = p[0] as i32;
    let remote_port = u16::from_be_bytes([p[6], p[7]]);
    let dst = &p[24..40];
    let remote = match family {
        libc::AF_INET => IpAddr::V4(Ipv4Addr::new(dst[0], dst[1], dst[2], dst[3])),
        libc::AF_INET6 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(dst);
            let v6 = Ipv6Addr::from(a);
            // Dual-stack sockets report IPv4 peers as ::ffff:a.b.c.d.
            match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(v6),
            }
        }
        _ => return None,
    };
    let cookie = u64::from(u32_ne(&p[44..48])) | u64::from(u32_ne(&p[48..52])) << 32;
    let inode = u32_ne(&p[68..72]);

    let (mut bytes_sent, mut bytes_received) = (None, None);
    let mut attrs = &p[DIAG_MSG_LEN..];
    while attrs.len() >= 4 {
        let len = u16_ne(&attrs[0..2]) as usize;
        let kind = u16_ne(&attrs[2..4]);
        if len < 4 || len > attrs.len() {
            break;
        }
        let value = &attrs[4..len];
        if kind == INET_DIAG_INFO
            && protocol == Protocol::Tcp
            && value.len() >= TCP_INFO_BYTES_RECEIVED + 8
        {
            bytes_sent = Some(u64_ne(&value[TCP_INFO_BYTES_ACKED..]));
            bytes_received = Some(u64_ne(&value[TCP_INFO_BYTES_RECEIVED..]));
        }
        attrs = &attrs[align4(len).min(attrs.len())..];
    }

    Some(RawSocket {
        protocol,
        inode,
        cookie,
        remote,
        remote_port,
        bytes_sent,
        bytes_received,
    })
}

/// `socket:[12345]` -> 12345
fn socket_inode(link: &str) -> Option<u32> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn process_name(pid: u32) -> String {
    // The executable name is not truncated to 15 characters like `comm` is.
    if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
            return name.trim_end_matches(" (deleted)").to_string();
        }
    }
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| format!("pid {pid}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn parses_socket_links() {
        assert_eq!(socket_inode("socket:[4242]"), Some(4242));
        assert_eq!(socket_inode("/dev/null"), None);
        assert_eq!(socket_inode("pipe:[12]"), None);
    }

    #[test]
    fn rejects_truncated_messages() {
        let mut msg = vec![0u8; NLMSG_HDR_LEN];
        msg[0..4].copy_from_slice(&200u32.to_ne_bytes());
        assert!(parse_messages(&msg, Protocol::Tcp, &mut Vec::new()).is_err());
    }

    /// End to end against the real kernel: open a connection, push bytes
    /// through it, and expect to find it attributed to this very process.
    #[test]
    fn sees_own_connection_with_byte_counts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (mut server, _) = listener.accept().unwrap();

        let payload = vec![7u8; 50_000];
        client.write_all(&payload).unwrap();
        let mut sink = vec![0u8; payload.len()];
        server.read_exact(&mut sink).unwrap();

        let mut collector = NetlinkCollector::new().unwrap();
        let flows = collector.poll().unwrap();
        let flow = flows
            .iter()
            .find(|f| f.remote_port == port && f.protocol == Protocol::Tcp)
            .expect("client socket not found in the dump");

        assert_eq!(flow.pid, Some(std::process::id()));
        assert!(
            flow.app.starts_with("phonehome"),
            "unexpected process name {}",
            flow.app
        );
        assert!(flow.bytes_sent.unwrap() >= payload.len() as u64);
        assert!(flow.remote.is_loopback());
    }
}
