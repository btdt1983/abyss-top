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
┌ Guests (sorted by CPU) ──────────────────────────────────────────────────────────────────────┐
│ PID    VM Name               STATE  LIBVIRT  UPTIME   CPU %   MEM (MB)   DISK R/W            NET RX/TX      │
│ 12847  cerberus-node01       up     run      4d2h      87.3       8192   12.4 MB/s / 3.1 MB/s  870 KB/s / 410 KB/s │
│ 12931  abyssos-build-runner  up     pause    18h30m    42.1       4096    2.1 MB/s / 5.6 MB/s  120 KB/s /  85 KB/s │
│ 13102  rocky9-test-vm        io     run      3h5m       3.4       2048      45 KB/s /  12 KB/s    8 KB/s /   2 KB/s │
│                                                                                                          │
│ TOTAL  3 VMs                                           132.8      14336   14.5 MB/s / 8.7 MB/s  998 KB/s / 497 KB/s │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
┌────────────────────────────────────────────────────────────────────────────┐
│ Host CPU  44.2%   RAM 14336/32768 MB (43.8%)   sort [C]pu [D]isk [N]et [M]em · [Q]uit │
└────────────────────────────────────────────────────────────────────────────┘
```

Colours in the live UI:

- **Abyss-Top** title — cyan bold; VM count badge yellow
- Table header — black on cyan
- STATE cell — green (`up`), yellow (`io`), red (`stop` / `dead`)
- LIBVIRT cell — green (`run` / `idle`), yellow (`pause` / `pmsus` / `down`),
  red (`off` / `crash`), gray (`none`), gray `-` when no libvirt data is
  available
- CPU % cell — green (<40), yellow (40–80), red (≥80)
- Disk R/W column — magenta; Net RX/TX column — light blue
- TOTAL row — bold, keeping each column's colour
- Footer host stats — white bold on a dark-gray block

---

## Features

- Auto-discovers QEMU/KVM processes (`qemu-system-*`, `qemu-kvm`, `kvm`)
- Parses the VM name from QEMU argv (`-name guest=foo,debug-threads=on`,
  `-name foo`, `--name foo`, `-name=foo`)
- Per-VM CPU%, RSS, disk read/write throughput, network RX/TX throughput
- Per-VM state (`up` / `io` / `stop` / `dead`) and uptime
- Per-VM **libvirt domain state** (`run` / `idle` / `pause` / `pmsus` /
  `down` / `off` / `crash` / `none`), polled from `virsh list --all` in the
  background every ~5 s — distinct from the process-based STATE column
  above (see [How it sees VMs](#how-it-sees-vms)); shows `-` and stays
  passive if `virsh`/`libvirtd` aren't available
- A `TOTAL` row summing CPU / memory / disk / net across all guests
- Host CPU + RAM totals in the footer
- 1 s refresh, sub-millisecond rendering, single binary
- Live sorting by CPU / disk I/O / net I/O / memory (`C` / `D` / `N` / `M`)
- vCPU / NUMA placement + host-contention detail view (`Enter` on a guest)
- Panic-safe terminal restore — your shell will never be left in raw mode

---

## FIPS posture

This binary performs local `/proc` telemetry, QEMU argv inspection, and an
optional local `virsh` subprocess call (a UNIX-socket connection to
`libvirtd`, no network, no TLS). It links no cryptographic crates.

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

### Libvirt domain-state column (optional)

The `LIBVIRT` column needs the `libvirt-client` package (`virsh`) installed
and a reachable local `libvirtd` (`qemu:///system` — the same privilege
ballpark as everything else on this page: root, or a user in the `libvirt`
group). It polls in the background every ~5 s and never blocks the UI.

- If `virsh` isn't installed at all, the column shows `-` for every guest
  and the feature disables itself for the rest of the run — no retries, no
  error spam.
- If `virsh` runs but can't reach `libvirtd` (e.g. mid-restart), that one
  poll is skipped and the last-known state is kept; it retries again in
  ~5 s — a brief hiccup doesn't blind the column for the rest of the
  session.

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
| State               | `sysinfo` process status (the state field of `/proc/[pid]/stat`): `up` (Run/Sleep/Idle), `io` (uninterruptible disk-sleep — a possible storage stall), `stop` (SIGSTOP'd or traced), `dead` (zombie/dead). This is the **OS process state, not the libvirt domain state** — a libvirt-*paused* guest keeps its QEMU process in Sleep, so it still reads `up`. What it surfaces is a hung, frozen, or defunct QEMU process, using only `/proc` (no QMP/libvirt). |
| Libvirt domain state | `virsh list --all`, polled on a background thread every ~5 s (never blocking the UI, even if `libvirtd` is wedged) and joined to a guest **by VM name** (the same name parsed from QEMU `-name`, which is the libvirt domain name for libvirt-managed guests). This is the real domain state — a `paused` guest reads `pause` here even though STATE still shows `up`. If `virsh` is missing entirely the column permanently falls back to `-`; a transient connection failure just keeps the last-known value until the next poll. |
| Uptime              | `sysinfo` process run time (`/proc/[pid]/stat` start time vs. host boot) |
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
heuristic, the sort-mode comparator, and the `virsh list --all` output
parser (canned string fixtures — no live `libvirtd` required to run the
suite).

---

## License

Apache-2.0.
