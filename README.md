# abyss-top

**Lightweight KVM/QEMU hypervisor TUI monitor for RHEL.**

`abyss-top` is a single-binary terminal UI that discovers running QEMU/KVM
guests on a Linux hypervisor and shows live CPU, memory, disk I/O and network
throughput per VM. It is designed for RHEL 9 / Rocky Linux / AlmaLinux hosts
where deploying heavy GUI monitoring stacks isn't an option (air-gapped sites,
FIPS-locked environments, SSH-only ops).

```
┌────────────────────────────────────────────────────────────────────────────┐
│ Abyss-Top | KVM Hypervisor Monitor  [3 VMs]                                │
└────────────────────────────────────────────────────────────────────────────┘
┌ Guests (sorted by CPU) ────────────────────────────────────────────────────┐
│ PID    VM Name                CPU %   MEM (MB)   DISK R/W           NET RX/TX        │
│ 12847  cerberus-node01         87.3       8192   12.4 MB/s / 3.1 MB/s    870 KB/s /  410 KB/s │
│ 12931  abyssos-build-runner    42.1       4096    2.1 MB/s / 5.6 MB/s    120 KB/s /   85 KB/s │
│ 13102  rocky9-test-vm           3.4       2048      45 KB/s /   12 KB/s    8 KB/s /    2 KB/s │
│                                                                                       │
└────────────────────────────────────────────────────────────────────────────┘
┌────────────────────────────────────────────────────────────────────────────┐
│ Host CPU  44.2%   RAM 14336/32768 MB (43.8%)   sort [C]pu [D]isk [N]et [M]em · [Q]uit │
└────────────────────────────────────────────────────────────────────────────┘
```

Colours in the live UI:

- **Abyss-Top** title — cyan bold; VM count badge yellow
- Table header — black on cyan
- CPU % cell — green (<40), yellow (40–80), red (≥80)
- Disk R/W column — magenta; Net RX/TX column — light blue
- Footer host stats — white bold on a dark-gray block

---

## Features

- Auto-discovers QEMU/KVM processes (`qemu-system-*`, `qemu-kvm`, `kvm`)
- Parses the VM name from QEMU argv (`-name guest=foo,debug-threads=on`,
  `-name foo`, `--name foo`, `-name=foo`)
- Per-VM CPU%, RSS, disk read/write throughput, network RX/TX throughput
- Host CPU + RAM totals in the footer
- 1 s refresh, sub-millisecond rendering, single binary
- Live sorting by CPU / disk I/O / net I/O / memory (`C` / `D` / `N` / `M`)
- Panic-safe terminal restore — your shell will never be left in raw mode

---

## FIPS posture

This binary performs **only local `/proc` telemetry and QEMU argv inspection.**
It links no cryptographic crates.

If TLS/HTTPS telemetry export is added later, it **must** dynamically link
against the host's FIPS-validated OpenSSL (RHEL system OpenSSL in FIPS mode)
or use `aws-lc-rs` with the `fips` feature enabled. Pure-Rust crypto crates
(`ring`, RustCrypto) are not permitted.

The release workflow builds a statically linked musl binary so it drops onto a
FIPS-enforcing host without dragging in a non-validated TLS stack.

---

## Installing

### From a release (recommended)

Download the latest tarball from the [Releases page](../../releases) and
extract it on the hypervisor:

```bash
curl -LO https://github.com/<owner>/abyss-top/releases/latest/download/abyss-top-x86_64-linux.tar.gz
tar -xzf abyss-top-x86_64-linux.tar.gz
sudo install -m 0755 abyss-top /usr/local/bin/abyss-top
```

Verify:

```bash
file /usr/local/bin/abyss-top
# ELF 64-bit LSB executable, x86-64, statically linked, ...
```

### From an RPM (RHEL 9 / Rocky 9 / AlmaLinux 9)

Grab the `.rpm` from the [Releases page](../../releases):

```bash
sudo dnf install ./abyss-top-1.0.0-1.el9.x86_64.rpm
```

The RPM installs `/usr/bin/abyss-top` and runs `setcap cap_dac_read_search=ep`
in `%post`, so any user in the `libvirt` group can see per-VM disk I/O without
needing root.

Verify:

```bash
rpm -qi abyss-top
rpm -qV abyss-top                # checks file integrity
getcap /usr/bin/abyss-top        # → cap_dac_read_search=ep
```

Uninstall:

```bash
sudo dnf remove abyss-top
```

### From source on RHEL / Rocky / Alma

```bash
# Build deps (just a C compiler — no openssl-devel, no pkg-config needed).
sudo dnf install -y gcc

# Rust via rustup (RHEL's packaged rust is often too old for the crates used).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Build & install.
git clone https://github.com/<owner>/abyss-top.git
cd abyss-top
cargo build --release
sudo install -m 0755 target/release/abyss-top /usr/local/bin/abyss-top
```

### Building an RPM yourself on RHEL

```bash
sudo dnf install -y gcc rpm-build rpmdevtools
rpmdev-setuptree

VERSION=1.0.0
git archive --format=tar.gz --prefix=abyss-top-${VERSION}/ \
  -o ~/rpmbuild/SOURCES/abyss-top-${VERSION}.tar.gz HEAD
cp packaging/abyss-top.spec ~/rpmbuild/SPECS/

rpmbuild -ba ~/rpmbuild/SPECS/abyss-top.spec
# → ~/rpmbuild/RPMS/x86_64/abyss-top-1.0.0-1.el9.x86_64.rpm
# → ~/rpmbuild/SRPMS/abyss-top-1.0.0-1.el9.src.rpm
```

The spec runs `cargo test` during `%check`, so a broken build will fail RPM
creation — you can't accidentally ship a binary that doesn't pass its tests.

### From source on any Linux

Requires Rust 1.75+:

```bash
git clone https://github.com/<owner>/abyss-top.git
cd abyss-top
cargo build --release
sudo install -m 0755 target/release/abyss-top /usr/local/bin/abyss-top
```

---

## Running on RHEL (capabilities, not root)

`/proc/[pid]/io` is restricted on RHEL: only root or the process owner can read
disk I/O counters. libvirt runs QEMU as `qemu:qemu`, so running `abyss-top` as
an unprivileged operator will show **0 B/s** for disk throughput across every
VM.

You have two reasonable options.

### Option A — run as root

Simple, expected for hypervisor ops. Just run:

```bash
sudo abyss-top
```

### Option B — grant `CAP_DAC_READ_SEARCH` (preferred for least-privilege)

A single capability is enough to read `/proc/[pid]/io` of other users without
giving the binary full root:

```bash
sudo setcap cap_dac_read_search=ep /usr/local/bin/abyss-top
```

Verify:

```bash
getcap /usr/local/bin/abyss-top
# /usr/local/bin/abyss-top cap_dac_read_search=ep
```

After this, any user in the `libvirt` group can run `abyss-top` and see live
per-VM disk and network throughput.

> **SELinux note.** On enforcing RHEL hosts the binary should be installed to a
> standard path (`/usr/local/bin` or `/usr/bin`) so the default `bin_t` label
> applies. If you place it elsewhere, restore the label with
> `restorecon -v <path>` or set it explicitly:
> `chcon -t bin_t <path>`. No custom policy module is required for the
> capability above.

---

## Keys

| Key       | Action |
|-----------|--------|
| `C`       | Sort guests by CPU % (default) |
| `D`       | Sort guests by disk I/O (read + write) |
| `N`       | Sort guests by net I/O (RX + TX) |
| `M`       | Sort guests by memory (RSS) |
| `Q` / `Esc` | Quit (terminal is always restored cleanly) |

The active sort is shown in the guest-table title (`Guests (sorted by …)`).

---

## How it sees VMs

| Signal              | Source                              |
|---------------------|-------------------------------------|
| Process discovery   | `sysinfo` — filters on `qemu-system*`, `qemu-kvm`, `kvm` (name + exe path) |
| VM name             | QEMU argv `-name` parser (all four common forms) |
| CPU %               | `sysinfo` per-process CPU usage — summed across all vCPU threads, so a busy multi-vCPU guest can read **above 100 %** (e.g. `780.0` for 8 fully-loaded vCPUs). This is intentional: it reflects real host load. |
| Memory (MB)         | `sysinfo` RSS / 1024²               |
| Disk R/W            | `/proc/[pid]/io` — `read_bytes` / `write_bytes`, delta per tick |
| Net RX/TX           | Each VM's own tap interface(s) are resolved **per PID** — from `/proc/[pid]/fd/*` links to the tap char device (`/dev/net/tun` for tun/tap bridges, `/dev/tapNN` for macvtap) whose `/proc/[pid]/fdinfo/<fd>` carries an `iff:` line (kernel tap driver), plus any explicit `ifname=` in the QEMU argv. Only those interfaces are summed from `/proc/[pid]/net/dev`, so a VM is billed for its own traffic and not the aggregate of every tap in the namespace. Counts are from the **host tap's perspective**, so guest egress appears as RX and guest ingress as TX. |

Per-PID interface resolution needs the same privilege as `/proc/[pid]/io` (root
or the process owner). If a VM's interfaces can't be resolved — a **separate
network namespace** (e.g. vhost-user with explicit netns), SR-IOV/PCI
passthrough, or missing privilege — its net stats fall back to 0 rather than
being mis-attributed. Standard libvirt bridge/tap and macvtap networking work
out of the box.

---

## Building a release locally

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
strip target/x86_64-unknown-linux-musl/release/abyss-top
```

The resulting binary is statically linked and runs on any modern Linux x86_64
host without glibc / runtime dependencies.

---

## Project layout

```
abyss-top/
├── Cargo.toml
├── README.md
├── LICENSE
├── .github/
│   └── workflows/
│       └── release.yml      # tagged builds → musl tarball + RPM → GH Release
├── packaging/
│   └── abyss-top.spec       # RPM spec for RHEL 9 / Rocky 9 / Alma 9
└── src/
    └── main.rs              # single-file app (TUI + /proc parser + tests)
```

---

## Tests

```bash
cargo test
```

Covers the QEMU argv parser, process detection heuristic, throughput delta
math (including counter-reset safety), byte formatter scaling, VM-interface
heuristic, and the sort-mode comparator.

---

## License

Apache-2.0.
