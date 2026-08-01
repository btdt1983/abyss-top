# Cargo.toml's release profile sets `strip = "symbols"`, so the binary ships
# pre-stripped. RHEL's default rpmbuild macros otherwise auto-spawn empty
# -debuginfo / -debugsource subpackages and fail with "Empty %files".
%global debug_package %{nil}

Name:           abyss-top
Version:        1.2.0
Release:        1%{?dist}
Summary:        Lightweight KVM/QEMU hypervisor TUI monitor for RHEL

License:        ASL 2.0
URL:            https://github.com/btdt1983/abyss-top
Source0:        %{name}-%{version}.tar.gz

# Geen runtime deps: musl-static build levert een zelfstandige ELF op.
# Voor de glibc-variant (BuildRequires: cargo) is alleen libc nodig, en die
# zit al in elke RHEL base install.
BuildRequires:  cargo >= 1.75
BuildRequires:  rust >= 1.75
BuildRequires:  gcc

# capability-setter; nodig voor de %post-stap die CAP_DAC_READ_SEARCH zet.
Requires:       libcap

ExclusiveArch:  x86_64 aarch64

%description
abyss-top is a single-binary terminal UI that discovers running QEMU/KVM
guests and shows live CPU, memory, disk I/O and network throughput per VM.
Designed for RHEL 9 / Rocky / AlmaLinux hypervisors where heavy GUI stacks
are not an option (air-gapped, FIPS-locked, SSH-only).

The binary performs only local /proc telemetry and links no cryptographic
libraries, keeping FIPS posture trivially auditable.

%prep
%autosetup

%build
# Offline build wanneer Source0 een vendored tarball is (zie %{name}-vendor).
# Voor de standaard build laten we cargo gewoon zijn werk doen.
cargo build --release --locked

%install
# Just the binary. README is handled by `%doc README.md` in %files (copied
# automatically into %{_docdir}/%{name}/), and LICENSE is handled by
# `%license LICENSE` (copied into %{_licensedir}/%{name}/). Doing an explicit
# `install -D ... %{_docdir}/%{name}/LICENSE` here duplicates the file and
# rpmbuild then refuses with "Installed (but unpackaged) file(s) found".
install -D -m 0755 target/release/%{name} %{buildroot}%{_bindir}/%{name}

%check
# Unit tests draaien tijdens de RPM-build; faalt de test, faalt de RPM.
cargo test --release --locked

%post
# CAP_DAC_READ_SEARCH + CAP_SYS_PTRACE zetten zodat unprivileged users in
# groep 'libvirt' QEMU-processen van een ANDERE Linux-user (bv. de libvirt
# 'qemu' service-account) kunnen inspecteren. Empirisch geverifieerd dat
# CAP_DAC_READ_SEARCH alleen NIET genoeg is voor /proc/<pid>/io,
# /proc/<pid>/fd/* (tap-resolution) of /proc/<pid>/numa_maps bij een andere
# eigenaar — die drie zijn ptrace-mode-gated (PTRACE_MODE_READ_FSCREDS) en
# vereisen CAP_SYS_PTRACE, ongeacht CAP_DAC_READ_SEARCH. Stil falen is OK
# (oudere libcap of read-only mount tijdens container-install) — de tool
# werkt dan nog steeds als root.
if [ -x /usr/sbin/setcap ]; then
    /usr/sbin/setcap cap_dac_read_search,cap_sys_ptrace=ep %{_bindir}/%{name} 2>/dev/null || :
fi

%postun
# Bij upgrade niets doen; bij volledige verwijdering hoeft setcap niet
# expliciet terug — het binary is dan al weg.
:

%files
%license LICENSE
%doc README.md
%{_bindir}/%{name}

%changelog
* Sat Aug 01 2026 David <david@cerberus.io> - 1.2.0-1
- New LIBVIRT column showing the real libvirt domain state, polled from
  `virsh list --all` on a background thread every ~5s (never blocking the
  UI, even if libvirtd is wedged). This closes the gap documented since
  1.1.0: the existing STATE column only ever reflected the QEMU OS process
  state, so a libvirt-"paused" guest still showed "up". If virsh isn't
  installed the column falls back to "-" and stays passive for the rest of
  the run; a transient libvirtd connection failure just skips that poll and
  keeps the last-known state. No new dependencies (stdlib thread + mpsc
  only).

* Tue Jul 21 2026 David <david@cerberus.io> - 1.1.0-1
- Guest table gains three columns/rows: per-VM STATE (the QEMU process state
  from /proc/<pid>/stat via sysinfo — up / io / stop / dead, so a hung,
  SIGSTOP'd or defunct guest stands out; note this is the OS process state,
  not the libvirt domain state, so a libvirt-"paused" guest still reads
  "up"), per-VM UPTIME (process run time), and a bottom TOTAL row summing
  CPU / memory / disk / net across all guests. Still pure /proc + /sys, no
  new dependencies.

* Tue Jul 14 2026 David <david@cerberus.io> - 1.0.0-2
- Per-vCPU NUMA placement + host-contention detail view: press Enter on a
  selected guest to see each vCPU thread's current physical CPU, NUMA node,
  and a schedstat-based "wait" contention percentage, plus guest memory
  NUMA locality (from /proc/<pid>/numa_maps). Pure /proc + /sys, no
  QMP/libvirt. Adds Up/Down row selection (was a flat table before).
- BUGFIX: CAP_DAC_READ_SEARCH alone was not sufficient to read
  /proc/<pid>/io, /proc/<pid>/fd/* (tap-resolution) or /proc/<pid>/numa_maps
  for a QEMU process owned by a DIFFERENT Linux user (e.g. the standard
  libvirt 'qemu' service account) — those are ptrace-mode-gated, not just
  DAC-gated. This silently zeroed disk/net throughput for non-root,
  setcap-only deployments whenever guests don't run under the same uid as
  abyss-top (root use masked it). %%post now also sets CAP_SYS_PTRACE.
