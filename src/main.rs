//! socket-vmnet-shim — drop-in replacement for cirruslabs/softnet
//!
//! Tart spawns `softnet` as a child process, passes a SOCK_DGRAM Unix
//! socketpair via stdin, and expects raw Ethernet frames (one frame per
//! syscall). This binary impersonates softnet's CLI surface but instead
//! of using vmnet.framework directly, it bridges to a running socket_vmnet
//! daemon (lima-vm/socket_vmnet) so that Tart VMs share the same L2
//! segment as Lima VMs.
//!
//!     ┌─ Tart VM ───────┐    raw datagrams    ┌─ shim ─┐  length-prefixed  ┌─ socket_vmnet ─┐
//!     │ virtio-net      │ ◄─────────────────► │        │ ◄───────────────► │ vmnet.framework│
//!     └─────────────────┘     SOCK_DGRAM      └────────┘     SOCK_STREAM   └────────────────┘
//!                                                                   │
//!                                                          flood frames to other clients
//!                                                                   │
//!                                                            ┌─ Lima VM(s) ─┐
//!
//! Wire formats:
//!  - VM side (Tart's socketpair, our stdin): SOCK_DGRAM, 1 frame per recv/send, no header.
//!  - socket_vmnet side (Unix stream): SOCK_STREAM, frames prefixed with 4-byte big-endian length.
//!
//! Path to socket_vmnet socket: $SOCKET_VMNET_PATH, default
//! `/private/var/run/lima/socket_vmnet.shared` (Lima's shared network).
//!
//! CLI compatibility with softnet:
//!  - Required (used): --vm-fd <int>
//!  - Tolerated (ignored, softnet-specific): --vm-mac-address, --vm-net-type,
//!    --allow, --block, --expose, --bootpd-lease-time, --user, --group.
//!
//! Lifecycle: exits cleanly on EOF/EPIPE on either side or on SIGINT.

use std::env;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::process::ExitCode;
use std::thread;

/// Default path of Lima's shared-mode socket_vmnet daemon.
const DEFAULT_SOCKET_VMNET_PATH: &str = "/private/var/run/lima/socket_vmnet.shared";

/// Maximum Ethernet frame size we'll buffer (jumbo-friendly).
const MAX_FRAME_SIZE: usize = 65536;

/// CLI flags that take a value (consumed via `--flag VAL` or `--flag=VAL`).
/// Anything outside this list is ignored.
const VALUE_FLAGS: &[&str] = &[
    "--vm-fd",
    "--vm-mac-address",
    "--vm-net-type",
    "--allow",
    "--block",
    "--expose",
    "--bootpd-lease-time",
    "--user",
    "--group",
];

fn main() -> ExitCode {
    let mut vm_fd: RawFd = 0; // Tart always passes fd=0 (stdin) per Softnet.swift

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];

        if raw == "-h" || raw == "--help" || raw == "--version" {
            print_help();
            return ExitCode::SUCCESS;
        }

        // Split `--flag=value` if needed.
        let (name, inline_val) = match raw.find('=') {
            Some(eq) if raw.starts_with("--") => (&raw[..eq], Some(raw[eq + 1..].to_string())),
            _ => (raw.as_str(), None),
        };

        if VALUE_FLAGS.iter().any(|f| *f == name) {
            let value = match inline_val {
                Some(v) => v,
                None => {
                    i += 1;
                    match args.get(i) {
                        Some(v) => v.clone(),
                        None => {
                            eprintln!("[shim] missing value for {}", name);
                            return ExitCode::from(2);
                        }
                    }
                }
            };
            if name == "--vm-fd" {
                match value.parse::<RawFd>() {
                    Ok(fd) => vm_fd = fd,
                    Err(_) => {
                        eprintln!("[shim] invalid --vm-fd value: {:?}", value);
                        return ExitCode::from(2);
                    }
                }
            }
            // Other VALUE_FLAGS are accepted but ignored (softnet packet filter).
        }
        // Unknown args are silently ignored for forward-compat with Tart.
        i += 1;
    }

    let smv_path =
        env::var("SOCKET_VMNET_PATH").unwrap_or_else(|_| DEFAULT_SOCKET_VMNET_PATH.to_string());

    eprintln!("[shim] vm_fd={} socket_vmnet_path={}", vm_fd, smv_path);

    // SAFETY: Tart passed us this FD as a SOCK_DGRAM Unix socketpair endpoint
    // (Softnet.swift:20-31). Taking ownership here means we'll close it on drop.
    let vm = unsafe { UnixDatagram::from_raw_fd(vm_fd) };

    let smv = match UnixStream::connect(&smv_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[shim] FATAL: cannot connect to socket_vmnet at {}: {}",
                smv_path, e
            );
            eprintln!("       Is the socket_vmnet daemon running? (Lima launches it");
            eprintln!("       on demand when a VM with the `shared` network starts.)");
            return ExitCode::from(3);
        }
    };

    eprintln!("[shim] connected to socket_vmnet, bridging frames...");

    let vm_w = vm.try_clone().expect("clone VM datagram FD");
    let smv_w = smv.try_clone().expect("clone socket_vmnet stream");

    // VM → socket_vmnet : read one frame, prepend 4-byte BE length, write to stream.
    let h_vm_to_smv = thread::Builder::new()
        .name("vm→smv".into())
        .spawn(move || -> std::io::Result<()> {
            let mut buf = vec![0u8; MAX_FRAME_SIZE];
            let mut smv = smv_w;
            loop {
                let n = vm.recv(&mut buf)?;
                if n == 0 {
                    // VM closed its end (shouldn't happen on SOCK_DGRAM but
                    // be safe — treat as graceful EOF).
                    return Ok(());
                }
                let hdr = (n as u32).to_be_bytes();
                // Combine header+payload into one writev-like call to avoid
                // interleaving with another VM's frames on the shared daemon.
                let mut out = Vec::with_capacity(4 + n);
                out.extend_from_slice(&hdr);
                out.extend_from_slice(&buf[..n]);
                smv.write_all(&out)?;
            }
        })
        .expect("spawn vm→smv");

    // socket_vmnet → VM : read 4-byte BE length, then frame, then send as datagram to VM.
    let h_smv_to_vm = thread::Builder::new()
        .name("smv→vm".into())
        .spawn(move || -> std::io::Result<()> {
            let mut smv = smv;
            let mut buf = vec![0u8; MAX_FRAME_SIZE];
            loop {
                let mut hdr = [0u8; 4];
                if let Err(e) = smv.read_exact(&mut hdr) {
                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                        return Ok(());
                    }
                    return Err(e);
                }
                let len = u32::from_be_bytes(hdr) as usize;
                if len == 0 || len > MAX_FRAME_SIZE {
                    eprintln!("[shim] suspicious frame length {} from socket_vmnet — aborting", len);
                    return Ok(());
                }
                smv.read_exact(&mut buf[..len])?;
                match vm_w.send(&buf[..len]) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        })
        .expect("spawn smv→vm");

    // Wait for either direction to terminate (Tart closes the FD, or socket_vmnet
    // closes the stream). Either case is treated as graceful exit.
    let _ = h_vm_to_smv.join();
    let _ = h_smv_to_vm.join();

    eprintln!("[shim] bridge closed, exiting");
    ExitCode::SUCCESS
}

fn print_help() {
    eprintln!(
        "socket-vmnet-shim {} — drop-in softnet replacement bridging Tart to socket_vmnet",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!();
    eprintln!("USAGE (as invoked by Tart):");
    eprintln!("    softnet --vm-fd 0 --vm-mac-address <mac> [--allow ...] [--block ...]");
    eprintln!();
    eprintln!("ENV:");
    eprintln!("    SOCKET_VMNET_PATH  Unix socket path of socket_vmnet daemon");
    eprintln!("                       (default: {})", DEFAULT_SOCKET_VMNET_PATH);
}
