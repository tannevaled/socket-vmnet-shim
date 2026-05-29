# socket-vmnet-shim

> **A drop-in replacement for [cirruslabs/softnet](https://github.com/cirruslabs/softnet) that bridges [Tart](https://github.com/cirruslabs/tart) VMs to a running [lima-vm/socket_vmnet](https://github.com/lima-vm/socket_vmnet) daemon, restoring VM↔VM connectivity on macOS Sequoia/Tahoe.**

~300 KB Rust binary, zero runtime dependencies, no patch to Tart or socket_vmnet required.

---

## The problem

On macOS Sonoma 14.x and later (especially **Sequoia 15** and **Tahoe 26**), Apple's `vmnet.framework` sets the kernel `PRIVATE` flag on every member interface of the `bridge100` bridge used by `--net-shared` and `--net-softnet`. The `PRIVATE` flag means the bridge refuses to forward L2 traffic between member ports — only between a member and the host. The user-space `ifconfig -private` toggle that previously worked around this has been removed from the binary.

The consequence: **two Tart VMs on the same Mac cannot ping each other.**

| Tart network mode | VM↔VM on Tahoe? |
|---|---|
| `--net-shared` (default) | ❌ blocked by bridge `PRIVATE` flag |
| `--net-softnet` | ❌ idem |
| `--net-softnet --net-softnet-allow=192.168.64.0/24` | ❌ softnet packet filter is bypassed, but the bridge isolation still blocks at L2 |
| `--net-host` | ❌ VM is isolated from everything (the point of the mode) |
| `--net-bridged=en0` | ✅ but VMs are exposed on the home/office LAN, depend on the router's DHCP, and trigger the macOS "Local Network" permission prompt |

For self-contained labs (no LAN exposure, reproducible IP plan), none of Tart's native modes work on recent macOS.

## How Lima solved it

Lima ships [`socket_vmnet`](https://github.com/lima-vm/socket_vmnet), a small C daemon that:
1. Holds the `vmnet.framework` interface itself (`vmnet_start_interface`).
2. Exposes a Unix domain socket.
3. **Floods every received Ethernet frame to every connected client in userspace, *before* it touches the kernel bridge.**

Because inter-VM frames are copied client-to-client by the daemon rather than traversing the bridge, the `PRIVATE` flag becomes irrelevant for that traffic. Two Lima VMs sharing the same daemon ping each other reliably on Sequoia and Tahoe (0% loss, ~0.5 ms RTT).

**Tart cannot use `socket_vmnet` natively.** The two systems speak incompatible wire formats:

| Side | Format |
|---|---|
| `socket_vmnet` | Unix **SOCK_STREAM**, 4-byte big-endian length prefix per frame (QEMU's `-netdev socket` convention) |
| Tart's `VZFileHandleNetworkDeviceAttachment` | Unix **SOCK_DGRAM**, one frame per `recv()`/`send()`, no header |

[`socket_vmnet` issue #13](https://github.com/lima-vm/socket_vmnet/issues/13) has tracked this for a while; no implementation existed before this shim.

## What this shim does

It impersonates `softnet`'s exact CLI surface (verified against `cirruslabs/tart` `Sources/tart/Network/Softnet.swift` and `cirruslabs/softnet` `lib/vm.rs`), so Tart launches it unmodified via `tart run --net-softnet <vm>`. Internally, it translates between the two wire formats:

```
   ┌─ Tart VM ───────┐    raw datagrams    ┌─ shim ─┐  4-byte BE length-prefix  ┌─ socket_vmnet ─┐
   │ virtio-net      │ ◄─────────────────► │        │ ◄───────────────────────► │ vmnet.framework│
   └─────────────────┘ SOCK_DGRAM (stdin)  └────────┘     SOCK_STREAM            └────────────────┘
                                                                  │
                                                        flood frames to other clients
                                                                  │
                                                          ┌─ Lima VMs ─┐
                                                          │            │
                                                          │  same /24  │  ← Tart and Lima VMs
                                                          │            │     coexist here
                                                          └────────────┘
```

All clients of the same `socket_vmnet` daemon — whether Lima VMs (QEMU+VZ) or Tart VMs (VZ) via this shim — end up on the same `/24` and can communicate.

## Verified results

Tested on **macOS Tahoe 26.5** (Apple Silicon, M-series, Tart 2.32.1, Lima 1.2.1, `socket_vmnet` 1.2.2):

- 3× Tart Debian 13 VMs via `--net-softnet` (shim) → all on `192.168.105.0/24`, full mesh ping ✅
- 3× Lima Debian 12 VMs + 1× Tart Debian VM on the same `socket_vmnet` daemon → mesh ping all 4 ✅
- 1× Tart macOS Tahoe-base VM + 3× Linux dc VMs on the same network → mesh ping ✅
- Static IPs assignable per VM via netplan (Linux) / `networksetup -setmanual` (macOS guest)

## Prerequisites

- **[Tart](https://github.com/cirruslabs/tart)** — `brew install cirruslabs/cli/tart` or [via pkgx](https://pkgx.dev/pkgs/tart.run): `pkgx +tart.run -- tart …`
- **[`socket_vmnet`](https://github.com/lima-vm/socket_vmnet)** installed **at `/opt/socket_vmnet/bin/socket_vmnet`** (root-owned). See [Installing socket_vmnet at the right path](#installing-socket_vmnet-at-the-right-path) below.
- A **running `socket_vmnet` daemon**. Several ways to get one:
  - Start any Lima VM configured with `networks: [{lima: shared}]` — Lima starts the daemon on demand.
  - Or run it system-wide via a LaunchDaemon (recommended for a Tart-only setup).
  - Or run it manually with the same flags Lima would use (see [`socket_vmnet`'s README](https://github.com/lima-vm/socket_vmnet#usage)).
- **A Rust toolchain** to build, e.g. via [pkgx](https://pkgx.sh/) (`pkgx +rust-lang.org cargo …`), [rustup](https://rustup.rs/), or `brew install rust`.

### Installing socket_vmnet at the right path

The `socket_vmnet` daemon must live at `/opt/socket_vmnet/bin/socket_vmnet` even though most package managers install it elsewhere. The fixed path is a Lima security requirement (the `/etc/sudoers.d/lima` rules name that exact path; sudo refuses anything user-writable) and our `LaunchDaemon` plist (if you use one) references it too.

The one-liner (auto-detects brew, pkgm, or `$PATH`):

```sh
./scripts/install-socket-vmnet.sh
# or pass an explicit source:
./scripts/install-socket-vmnet.sh /path/to/your/socket_vmnet
```

What it does under the hood — same as if you ran it by hand:

**Homebrew**

```sh
brew install socket_vmnet
# The keg-only formula leaves the binary under /opt/homebrew/opt/socket_vmnet/bin/.
sudo install -m 0755 -o root -g wheel -d /opt/socket_vmnet/bin
sudo install -m 0755 -o root -g wheel \
  /opt/homebrew/opt/socket_vmnet/bin/socket_vmnet \
  /opt/socket_vmnet/bin/socket_vmnet
sudo install -m 0755 -o root -g wheel \
  /opt/homebrew/opt/socket_vmnet/bin/socket_vmnet_client \
  /opt/socket_vmnet/bin/socket_vmnet_client
```

**pkgx (once [pkgxdev/pantry#13093](https://github.com/pkgxdev/pantry/pull/13093) is merged)**

```sh
pkgm install github.com/lima-vm/socket_vmnet
# pkgm symlinks the binary to /usr/local/bin/socket_vmnet
sudo install -m 0755 -o root -g wheel -d /opt/socket_vmnet/bin
sudo install -m 0755 -o root -g wheel \
  /usr/local/bin/socket_vmnet /opt/socket_vmnet/bin/socket_vmnet
sudo install -m 0755 -o root -g wheel \
  /usr/local/bin/socket_vmnet_client /opt/socket_vmnet/bin/socket_vmnet_client
```

**Manual / from source**

The upstream Makefile's default `make install.bin` target already installs to `/opt/socket_vmnet/` (its `PREFIX ?= /opt/socket_vmnet`):

```sh
git clone https://github.com/lima-vm/socket_vmnet
cd socket_vmnet
make
sudo make install.bin
# Yields /opt/socket_vmnet/bin/{socket_vmnet,socket_vmnet_client}, root-owned.
```

After install, also lay down Lima's sudoers entries (they grant the `everyone` group permission to start/stop the daemon without a password — this is what makes `socket_vmnet` usable without per-VM sudo prompts):

```sh
limactl sudoers > /tmp/lima.sudoers
sudo install -o root -m 0440 /tmp/lima.sudoers /etc/sudoers.d/lima
sudo chmod 0644 /etc/sudoers.d/lima   # so Lima/our tasks can read it for validation
```

## Build

```sh
git clone https://github.com/tannevaled/socket-vmnet-shim
cd socket-vmnet-shim
cargo build --release            # or: pkgx +rust-lang.org cargo build --release
```

Output: `target/release/softnet` (~300 KB, statically linked against `std` only — no external crates).

## Install

Tart resolves `softnet` via `$PATH` (Swift `resolveBinaryPath("softnet")` in `Softnet.swift`). The shim binary is named `softnet` precisely so it can shadow the original Homebrew `softnet` via PATH order.

**Personal install** (no system changes, recommended for a dev box):

```sh
mkdir -p ~/.local/bin
install -m 0755 target/release/softnet ~/.local/bin/softnet
# Make ~/.local/bin first in PATH (add to ~/.zprofile, ~/.zshrc, etc.)
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile
```

**SUID-root step (one-time):** Tart's `Softnet.swift` (lines 101-158) checks that the resolved `softnet` binary is owned by root with the SUID bit set; if it isn't, Tart auto-spawns `sudo chown root <path> && chmod u+s <path>` before continuing. That auto-escalation **blocks on a sudo password prompt** in whatever terminal launched Tart, and the VM never reaches `running` state. To avoid the hang, SUID the shim yourself once at install time:

```sh
sudo chown root ~/.local/bin/softnet
sudo chmod u+s ~/.local/bin/softnet
```

The shim itself doesn't *need* root (it only opens a Unix socket that's already group-`everyone` accessible), but the SUID bit shuts Tart's elevation logic up. The binary running as root has no extra capability it actually uses.

**System-wide install** (multi-user setup; both `which softnet` defaults to the shim for every shell):

```sh
sudo install -m 4755 -o root -g wheel target/release/softnet /usr/local/bin/softnet
# Make sure /usr/local/bin is before /opt/homebrew/bin in PATH if Homebrew's softnet is also installed.
```

## Usage

Once installed, use Tart's normal `--net-softnet` flag — no configuration change:

```sh
tart run --net-softnet my-vm
```

The shim runs as the invoking user (the SUID-root simply makes Tart happy; it doesn't enable anything functional). The `socket_vmnet` daemon already runs as root with `--socket-group=everyone`, so unprivileged connections are accepted.

### Pointing the shim at a non-default socket

By default the shim connects to `/private/var/run/lima/socket_vmnet.shared` (Lima's standard shared-network socket). To use a different daemon — e.g., one running on `--vmnet-mode=host` or in a separate location — set `SOCKET_VMNET_PATH`:

```sh
SOCKET_VMNET_PATH=/var/run/my-socket-vmnet.sock tart run --net-softnet my-vm
```

The variable must be set in the environment of the process that launches `tart` (Tart will inherit it; Tart's child `softnet` invocation does too).

## CLI compatibility

The shim accepts every flag the original `softnet` accepts so Tart's argv works unchanged:

| Flag | Behavior |
|---|---|
| `--vm-fd <N>` | Used. Tart always passes `0` (stdin). |
| `--vm-mac-address <mac>` | Ignored. `socket_vmnet` manages MAC/DHCP internally. |
| `--vm-net-type {nat,host}` | Ignored. The mode is determined by the `socket_vmnet` daemon you connect to. |
| `--allow <cidrs>` | Ignored. (`softnet`'s packet filter — not relevant for a pure bridge.) |
| `--block <cidrs>` | Ignored. |
| `--expose <ports>` | Ignored. |
| `--bootpd-lease-time <s>` | Ignored. |
| `--user`, `--group` | Ignored. |

Unrecognised flags are silently consumed (one or two values per flag depending on heuristic) for forward compatibility with newer Tart versions.

## Limitations

- **Requires a running `socket_vmnet` daemon.** The shim is a bridge, not a daemon. If `socket_vmnet` isn't up, the shim exits with code 3 on startup and Tart fails.
- **No packet filtering.** If you used `--net-softnet-allow` / `--net-softnet-block` with original `softnet` to firewall a VM, those rules are *not* enforced here. Whatever `socket_vmnet` permits, the shim forwards.
- **Single network per VM.** Each VM connects to one `socket_vmnet` daemon, chosen via `SOCKET_VMNET_PATH`. To put VMs on different isolated networks, run separate daemons on different socket paths.
- **No re-connect.** If `socket_vmnet` dies while the shim is running, the shim exits and Tart loses the link. Restart the VM after restarting the daemon. (Trivial to add a reconnect loop; PRs welcome.)

## Architecture

Single file, ~200 lines of Rust, `std`-only. Two threads, one per direction. Direct blocking I/O — no async runtime needed for two streams.

The Tart↔shim contract (verified by reading `tart` and `softnet` source):

- Tart creates the socketpair: `socketpair(AF_UNIX, SOCK_DGRAM, 0, &fds)`. One end becomes the VM's `VZFileHandleNetworkDeviceAttachment`; the other end is wired to `softnet`'s `stdin` (`fd 0`). One `recv()` returns exactly one Ethernet frame; one `send()` writes exactly one frame. `Softnet.swift` sets `SO_SNDBUF=1 MiB` / `SO_RCVBUF=4 MiB` on the VM side.
- argv at spawn time: `softnet --vm-fd 0 --vm-mac-address <mac> [--allow <cidrs>] [--block <cidrs>] [--expose <specs>]`.
- Lifecycle: Tart sends `SIGINT` to shut down; the shim's threads exit on `recv() == 0` / `read_exact` EOF / `EPIPE` / `BrokenPipe`.

The `socket_vmnet` wire format (verified by reading `lima-vm/socket_vmnet/main.c`):

- Unix `SOCK_STREAM` connection to the socket file.
- Each frame is prefixed by 4 bytes of big-endian length (`htonl(frame_len)`), then the frame bytes.

See [`src/main.rs`](src/main.rs) — fully annotated.

## Frequently asked

**Why not just patch Tart to support `socket_vmnet` natively?**

Eventually that would be cleaner. But:
- Tart is Swift and would need to expose a new flag (`--net-socket-vmnet=<path>`) and reuse its existing `VZFileHandleNetworkDeviceAttachment` plumbing.
- The framing conversion still has to happen somewhere — either in-process in Tart or in a sidecar. A sidecar is what `softnet` already is.
- A drop-in `softnet` replacement requires zero coordination with cirruslabs and unblocks users today.

If/when Tart adds native support, this shim can be retired with no fuss.

**Why not just patch `socket_vmnet` to support `VZFileHandleNetworkDeviceAttachment` directly?**

This is what [`socket_vmnet` issue #13](https://github.com/lima-vm/socket_vmnet/issues/13) proposes. It would require `socket_vmnet` to grow a new wire format (raw datagrams over `SOCK_DGRAM`), bifurcating its protocol surface and complicating multiplexing logic that currently assumes length-prefixed streams. A separate bridge tool sidesteps that complexity.

**Is the SUID step a security risk?**

Practically, no:
- The shim doesn't `setuid(0)` and doesn't act on root privilege. It opens stdin (already given by Tart) and one Unix client socket (`socket_vmnet` socket has group `everyone` so a normal user can connect anyway). Its effective UID would be root but it never uses it.
- Anyone who can write to `~/.local/bin/softnet` could replace it with a SUID-root malware launcher. For a single-user laptop this is fine; for multi-user systems install in `/usr/local/bin` (root-owned, world-readable).
- The shim is small enough to audit (~200 lines, std-only) — feel free to read [`src/main.rs`](src/main.rs) before SUID-ing.

If you'd rather avoid SUID entirely, an alternative is to add a sudoers `NOPASSWD` entry covering Tart's auto-escalation (`chown root /path/to/softnet` and `chmod u+s /path/to/softnet`). That's another route, untested in this README.

**What about Sequoia 15 (vs Tahoe 26)?**

The `PRIVATE` flag was already set on Sequoia. The shim has been written and tested on Tahoe but should work identically on Sequoia. Reports welcome.

**Performance?**

I/O-bound — the shim does memcpy-and-pipe in userspace. On Apple Silicon M-series, VM↔VM ping inside the same daemon is ~0.4–0.6 ms RTT, comparable to native `socket_vmnet` Lima↔Lima. Bulk throughput hasn't been benchmarked rigorously; expect parity with `socket_vmnet` since the shim is just a copy step between two Unix sockets.

## Status

**Proof of concept**, used on a personal lab. The code is small and the wire contracts are precisely specified, so it's unlikely to break across Tart minor versions. If Tart changes how it spawns `softnet`, the shim may need an argv-parser tweak. If `socket_vmnet` changes its wire format (unlikely, since QEMU compatibility is its raison d'être), the framing code needs an update.

Issues and PRs welcome.

## Acknowledgements

- **[cirruslabs](https://github.com/cirruslabs)** — Tart, `softnet`. The latter's open Rust source made it trivial to derive the exact wire contract.
- **[lima-vm/socket_vmnet](https://github.com/lima-vm/socket_vmnet)** — the daemon that does the actual hard work. The shim is just a translator.
- **macOS team at Apple** — for `vmnet.framework`, despite the `PRIVATE` flag ruining the day. (And specifically for `VZFileHandleNetworkDeviceAttachment`, which makes this kind of plumbing possible.)

## License

MIT OR Apache-2.0
