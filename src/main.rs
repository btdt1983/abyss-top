// abyss-top — KVM/QEMU Hypervisor TUI Monitor
//
// Doelplatform: RHEL 9 / Rocky / AlmaLinux (FIPS-capable hosts).
// FIPS-houding: deze binary doet ALLEEN lokale /proc-telemetrie en QEMU
// argv-inspectie. Geen crypto-crates. Wordt later TLS-export toegevoegd,
// dan MOET die dynamisch linken tegen de FIPS-gevalideerde OpenSSL van de
// host of `aws-lc-rs` met de `fips` feature. Pure-Rust crypto (ring,
// RustCrypto) is hier verboden.
//
// Cargo.toml dependencies:
//   anyhow    = "1.0"
//   crossterm = "0.27"
//   ratatui   = "0.26"
//   sysinfo   = "0.30"

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Stdout},
    panic,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

// Refresh-cadans. sysinfo CPU% vereist >= MINIMUM_CPU_UPDATE_INTERVAL tussen
// refreshes; 1 s zit ruim boven de ~200ms ondergrens en geeft stabiele delta's.
const TICK_RATE: Duration = Duration::from_millis(1000);

// ---------- Data-model ----------------------------------------------------

/// Ruwe cumulatieve I/O-tellers per proces (bytes sinds proces-start).
/// Worden gebruikt om delta's te berekenen tussen ticks.
#[derive(Debug, Clone, Copy, Default)]
struct IoCounters {
    disk_read: u64,
    disk_write: u64,
    net_rx: u64,
    net_tx: u64,
}

/// Snapshot van de vorige tick: tellers + tijdstip, nodig voor throughput.
#[derive(Debug, Clone, Copy)]
struct PrevSample {
    counters: IoCounters,
    at: Instant,
}

/// Snapshot van de vorige tick voor één vCPU-thread: schedstat-wachttijd
/// (ns), nodig om er een contentie-percentage van te maken (zelfde
/// delta-patroon als `PrevSample`, maar per (pid, tid) i.p.v. per pid).
#[derive(Debug, Clone, Copy)]
struct ThreadPrevSample {
    wait_ns: u64,
    at: Instant,
}

/// Eén vCPU-thread van de momenteel uitgeklapte VM: waar draait 'ie fysiek,
/// op welke NUMA-node, en hoezeer wacht 'ie op host-CPU (contentie).
#[derive(Debug, Clone, Copy)]
struct VCpuThread {
    tid: u32,
    vcpu_index: u32,
    cur_cpu: u32,
    numa_node: Option<u32>,
    wait_pct: f32,
}

/// Detail-data voor de uitgeklapte VM. Lazy berekend (alleen voor de
/// geselecteerde PID, niet elke tick voor alle VM's — zie `App::recompute_detail`).
#[derive(Debug, Clone)]
struct VmDetail {
    vcpus: Vec<VCpuThread>,
    /// (numa node, percentage van resident guest-pagina's op die node).
    mem_by_node: Vec<(u32, f32)>,
    /// True als het lezen van numa_maps op Permission denied liep
    /// (ontbrekende CAP_SYS_PTRACE) — de UI toont dan een uitlegregel i.p.v.
    /// stilzwijgend lege memory-locality-data.
    denied: bool,
}

#[derive(Debug, Clone)]
struct VmStats {
    pid: u32,
    name: String,
    cpu_pct: f32,
    mem_mb: u64, // RSS in MB

    // Throughput in bytes/s, berekend uit delta tussen twee ticks.
    disk_read_bps: u64,
    disk_write_bps: u64,
    net_rx_bps: u64,
    net_tx_bps: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct HostStats {
    cpu_pct: f32,
    mem_used_mb: u64,
    mem_total_mb: u64,
}

/// Sorteer-modus. Standaard op CPU; via de toetsen c/d/n/m live te wisselen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Cpu,
    DiskIo,
    NetIo,
    Memory,
}

impl SortBy {
    /// Korte label voor in de tabeltitel.
    fn label(self) -> &'static str {
        match self {
            SortBy::Cpu => "CPU",
            SortBy::DiskIo => "disk I/O",
            SortBy::NetIo => "net I/O",
            SortBy::Memory => "memory",
        }
    }
}

// ---------- App-state -----------------------------------------------------

struct App {
    system: System,
    vms: Vec<VmStats>,
    host: HostStats,
    /// Vorige I/O-snapshots per PID. Onbekende PID's (nieuwe VM, of net
    /// afgesloten) worden veilig afgehandeld: geen delta = throughput 0.
    prev: HashMap<u32, PrevSample>,
    sort_by: SortBy,
    should_quit: bool,
    /// Rij-cursor in de tabel (index in `vms`, niet PID — zelfde eenvoud als
    /// de rest van dit bestand; bij een sort-wissel kan de cursor dus naar
    /// een andere fysieke VM "springen", net als in de meeste minimalistische
    /// top-achtige tools).
    selected: usize,
    /// Toont de vCPU/NUMA-detail-view voor de geselecteerde VM.
    expanded: bool,
    /// Host-topologie fysieke-CPU -> NUMA-node. Statisch voor de levensduur
    /// van het proces; eenmalig opgebouwd in `new()`.
    cpu_to_node: HashMap<u32, u32>,
    /// Aantal DISTINCTE NUMA-nodes (niet `cpu_to_node.len()` — dat is het
    /// aantal CPU's, niet het aantal nodes). Eenmalig afgeleid in `new()`.
    node_count: usize,
    /// Vorige schedstat-wachttijd per (pid, tid), voor de contentie-delta
    /// van de uitgeklapte VM's vCPU-threads.
    prev_thread: HashMap<(u32, u32), ThreadPrevSample>,
    /// Lazy berekende detail-data voor de uitgeklapte VM (`None` als niet
    /// uitgeklapt, of de geselecteerde VM nog geen data heeft).
    detail: Option<VmDetail>,
    table_state: TableState,
}

impl App {
    fn new() -> Self {
        // System met CPU + geheugen + processen. Eerste new_all() vult de
        // baselines; de eerste CPU-sample wordt later in prime() weggegooid.
        let refresh = RefreshKind::new()
            .with_cpu(sysinfo::CpuRefreshKind::everything())
            .with_memory(sysinfo::MemoryRefreshKind::everything())
            .with_processes(ProcessRefreshKind::everything());
        let mut system = System::new_with_specifics(refresh);
        system.refresh_all();

        let mut table_state = TableState::default();
        table_state.select(Some(0));

        let cpu_to_node = build_cpu_to_node_map();
        let node_count = cpu_to_node.values().copied().collect::<HashSet<_>>().len();

        Self {
            system,
            vms: Vec::new(),
            host: HostStats::default(),
            prev: HashMap::new(),
            sort_by: SortBy::Cpu,
            should_quit: false,
            selected: 0,
            expanded: false,
            cpu_to_node,
            node_count,
            prev_thread: HashMap::new(),
            detail: None,
            table_state,
        }
    }

    /// Vult CPU-tellers en seed't I/O-baselines. Zonder dit toont de eerste
    /// frame overal 0% CPU en 0 B/s throughput.
    fn prime(&mut self) {
        self.system.refresh_cpu();
        self.system
            .refresh_processes_specifics(ProcessRefreshKind::everything().with_cpu());

        // Seed de I/O-baselines zodat de eerste echte refresh al delta's geeft.
        // Alleen QEMU-processen: die worden gerenderd, dus baselines voor de
        // rest van het systeem zijn verspild werk (en zouden read_io_counters
        // voor honderden processen aanroepen).
        let now = Instant::now();
        for (pid, p) in self.system.processes() {
            if !is_qemu_process(p.name(), p.exe().and_then(|e| e.to_str())) {
                continue;
            }
            let pid_u32 = pid.as_u32();
            let args: Vec<&str> = p.cmd().iter().map(|s| s.as_str()).collect();
            let ifaces = vm_ifaces_for_pid(pid_u32, &args);
            let counters = read_io_counters(pid_u32, &ifaces);
            self.prev.insert(pid_u32, PrevSample { counters, at: now });
        }

        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        self.refresh();
    }

    fn refresh(&mut self) {
        self.system.refresh_cpu();
        self.system.refresh_memory();
        self.system
            .refresh_processes_specifics(ProcessRefreshKind::everything().with_cpu());

        // Host-CPU = gemiddelde over alle cores.
        let cpus = self.system.cpus();
        let cpu_pct = if cpus.is_empty() {
            0.0
        } else {
            cpus.iter().map(|c| c.cpu_usage()).sum::<f32>() / cpus.len() as f32
        };
        self.host = HostStats {
            cpu_pct,
            mem_used_mb: self.system.used_memory() / 1024 / 1024,
            mem_total_mb: self.system.total_memory() / 1024 / 1024,
        };

        let now = Instant::now();
        let mut seen: Vec<u32> = Vec::with_capacity(self.prev.len());
        let mut vms: Vec<VmStats> = Vec::new();

        for (pid, p) in self.system.processes() {
            if !is_qemu_process(p.name(), p.exe().and_then(|e| e.to_str())) {
                continue;
            }
            let pid_u32 = pid.as_u32();
            seen.push(pid_u32);

            let args: Vec<&str> = p.cmd().iter().map(|s| s.as_str()).collect();
            let name = extract_vm_name(&args).unwrap_or_else(|| format!("pid:{}", pid_u32));

            // Resolve de eigen tap-interface(s) van deze VM en lees de huidige
            // cumulatieve I/O-tellers (disk + net, alleen die interfaces).
            let ifaces = vm_ifaces_for_pid(pid_u32, &args);
            let cur = read_io_counters(pid_u32, &ifaces);

            // Delta berekenen t.o.v. vorige sample. Als er geen vorige is
            // (nieuw ontdekte VM of vlak na prime), is throughput 0 deze tick.
            let (d_r, d_w, n_rx, n_tx) = match self.prev.get(&pid_u32) {
                Some(prev) => {
                    let dt = now.saturating_duration_since(prev.at).as_secs_f64().max(1e-3);
                    (
                        per_second(cur.disk_read, prev.counters.disk_read, dt),
                        per_second(cur.disk_write, prev.counters.disk_write, dt),
                        per_second(cur.net_rx, prev.counters.net_rx, dt),
                        per_second(cur.net_tx, prev.counters.net_tx, dt),
                    )
                }
                None => (0, 0, 0, 0),
            };

            // Sample opslaan voor volgende tick.
            self.prev.insert(pid_u32, PrevSample { counters: cur, at: now });

            vms.push(VmStats {
                pid: pid_u32,
                name,
                // sysinfo telt CPU% op over alle threads van het proces, dus een
                // VM met N vCPU's kan tot ~N*100% tonen (bv. 780.0 bij 8 vCPU's
                // vol belast). Bewust: het weerspiegelt de echte hostbelasting.
                cpu_pct: p.cpu_usage(),
                mem_mb: p.memory() / 1024 / 1024, // RSS
                disk_read_bps: d_r,
                disk_write_bps: d_w,
                net_rx_bps: n_rx,
                net_tx_bps: n_tx,
            });
        }

        // Oude entries uit prev opruimen: VM's die tijdens de tick zijn
        // gestopt blijven anders eeuwig in de map zitten (memory leak).
        if self.prev.len() > seen.len() * 2 {
            let seen_set: HashSet<u32> = seen.into_iter().collect();
            self.prev.retain(|pid, _| seen_set.contains(pid));
        }

        sort_vms(&mut vms, self.sort_by);
        self.vms = vms;

        // Cursor klemmen als de VM-lijst gekrompen is (VM gestopt).
        if self.selected >= self.vms.len() {
            self.selected = self.vms.len().saturating_sub(1);
        }
        self.table_state
            .select(if self.vms.is_empty() { None } else { Some(self.selected) });

        // Thread-samples opruimen voor VM's die niet meer bestaan. Klein
        // genoeg (enkel PID's die ooit zijn uitgeklapt) om elke tick te
        // filteren i.p.v. een groei-drempel zoals bij `prev` hierboven.
        if !self.prev_thread.is_empty() {
            let live: HashSet<u32> = self.vms.iter().map(|v| v.pid).collect();
            self.prev_thread.retain(|(pid, _), _| live.contains(pid));
        }

        if self.expanded {
            self.recompute_detail();
        }
    }

    fn on_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') | KeyCode::Char('C') => self.set_sort(SortBy::Cpu),
            KeyCode::Char('d') | KeyCode::Char('D') => self.set_sort(SortBy::DiskIo),
            KeyCode::Char('n') | KeyCode::Char('N') => self.set_sort(SortBy::NetIo),
            KeyCode::Char('m') | KeyCode::Char('M') => self.set_sort(SortBy::Memory),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Enter | KeyCode::Tab => self.toggle_expanded(),
            _ => {}
        }
    }

    /// Wissel de sorteer-modus en hersorteer meteen, zodat de tabel direct
    /// meebeweegt i.p.v. pas bij de volgende tick.
    fn set_sort(&mut self, by: SortBy) {
        if self.sort_by != by {
            self.sort_by = by;
            sort_vms(&mut self.vms, self.sort_by);
        }
    }

    /// Beweegt de rij-cursor (wrap-around), en herberekent meteen de
    /// detail-view als die open staat — zo kun je met ↑/↓ door VM's bladeren
    /// zonder telkens opnieuw Enter te drukken.
    fn move_selection(&mut self, delta: i32) {
        if self.vms.is_empty() {
            return;
        }
        let len = self.vms.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len) as usize;
        if next != self.selected {
            self.selected = next;
            self.table_state.select(Some(self.selected));
            if self.expanded {
                self.recompute_detail();
            }
        }
    }

    /// Toont/verbergt de vCPU/NUMA-detail-view voor de geselecteerde VM.
    /// Berekent meteen (niet pas volgende tick), zelfde "instant feedback"
    /// idee als `set_sort`.
    fn toggle_expanded(&mut self) {
        if self.vms.is_empty() {
            return;
        }
        self.expanded = !self.expanded;
        if self.expanded {
            self.recompute_detail();
        } else {
            self.detail = None;
        }
    }

    /// Herberekent de vCPU/NUMA-detail-data voor **alleen** de geselecteerde
    /// VM. Bewust niet voor alle VM's elke tick: task-directory-walks +
    /// numa_maps-parsing per VM zou de refresh-loop traag maken op hosts met
    /// veel guests.
    fn recompute_detail(&mut self) {
        let Some(vm) = self.vms.get(self.selected) else {
            self.detail = None;
            return;
        };
        let pid = vm.pid;
        let now = Instant::now();
        let mut vcpus = Vec::new();

        for tid in read_task_ids(pid) {
            let Some(comm) = read_thread_comm(pid, tid) else { continue };
            let Some(vcpu_index) = parse_vcpu_index(&comm) else { continue };
            let Some(cur_cpu) = read_thread_processor(pid, tid) else { continue };
            let numa_node = self.cpu_to_node.get(&cur_cpu).copied();

            let wait_ns = read_thread_schedstat_wait(pid, tid).unwrap_or(0);
            let wait_pct = match self.prev_thread.get(&(pid, tid)) {
                Some(prev) => {
                    let dt = now.saturating_duration_since(prev.at).as_secs_f64().max(1e-3);
                    let per_sec_ns = per_second(wait_ns, prev.wait_ns, dt);
                    ((per_sec_ns as f64 / 1e9) * 100.0).min(100.0) as f32
                }
                None => 0.0,
            };
            self.prev_thread.insert((pid, tid), ThreadPrevSample { wait_ns, at: now });

            vcpus.push(VCpuThread { tid, vcpu_index, cur_cpu, numa_node, wait_pct });
        }
        vcpus.sort_by_key(|v| v.vcpu_index);

        // Threads van déze VM die niet meer bestaan (vCPU-hotunplug is
        // zeldzaam maar niet onmogelijk) opruimen uit de cache.
        let seen_tids: HashSet<u32> = vcpus.iter().map(|v| v.tid).collect();
        self.prev_thread.retain(|(p, tid), _| *p != pid || seen_tids.contains(tid));

        let (mem_by_node, denied) = match read_numa_maps(pid) {
            Ok(totals) => {
                let total: u64 = totals.values().sum();
                if total == 0 {
                    (Vec::new(), false)
                } else {
                    let mut pct: Vec<(u32, f32)> = totals
                        .into_iter()
                        .map(|(node, pages)| (node, (pages as f64 / total as f64 * 100.0) as f32))
                        .collect();
                    pct.sort_by_key(|(node, _)| *node);
                    (pct, false)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => (Vec::new(), true),
            Err(_) => (Vec::new(), false),
        };

        self.detail = Some(VmDetail { vcpus, mem_by_node, denied });
    }
}

/// Veilige throughput-berekening: counter-resets (cur < prev, bv. na PID-reuse
/// of fork) leveren 0 op i.p.v. een onderloop-panic.
fn per_second(cur: u64, prev: u64, dt_secs: f64) -> u64 {
    if cur < prev {
        return 0;
    }
    let delta = (cur - prev) as f64;
    (delta / dt_secs).max(0.0) as u64
}

fn sort_vms(vms: &mut [VmStats], by: SortBy) {
    use std::cmp::Ordering;
    vms.sort_by(|a, b| match by {
        SortBy::Cpu => b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(Ordering::Equal),
        SortBy::DiskIo => {
            let ka = a.disk_read_bps + a.disk_write_bps;
            let kb = b.disk_read_bps + b.disk_write_bps;
            kb.cmp(&ka)
        }
        SortBy::NetIo => {
            let ka = a.net_rx_bps + a.net_tx_bps;
            let kb = b.net_rx_bps + b.net_tx_bps;
            kb.cmp(&ka)
        }
        SortBy::Memory => b.mem_mb.cmp(&a.mem_mb),
    });
}

// ---------- QEMU-detectie & -name parsing --------------------------------

/// True als dit een QEMU/KVM-hypervisorproces is.
/// `proc_name` is sysinfo's procesnaam (vaak afgekapt op 15 chars op Linux),
/// `exe` is het uitgeresolvde pad indien beschikbaar.
fn is_qemu_process(proc_name: &str, exe: Option<&str>) -> bool {
    let n = proc_name.to_ascii_lowercase();
    if n.contains("qemu-system") || n.starts_with("qemu-kvm") || n == "kvm" {
        return true;
    }
    if let Some(e) = exe {
        let e = e.to_ascii_lowercase();
        if e.contains("qemu-system") || e.ends_with("/qemu-kvm") || e.ends_with("/kvm") {
            return true;
        }
    }
    false
}

/// Haal de VM-naam uit QEMU argv. Ondersteunt:
///   -name guest=foo,debug-threads=on
///   -name foo
///   -name foo,debug-threads=on
///   --name foo
///   -name=foo
fn extract_vm_name(args: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        let raw = if a == "-name" || a == "--name" {
            i += 1;
            args.get(i).copied()?
        } else if let Some(rest) = a.strip_prefix("-name=").or_else(|| a.strip_prefix("--name=")) {
            Some(rest)?
        } else {
            i += 1;
            continue;
        };

        let first = raw.split(',').next().unwrap_or(raw).trim();
        let cleaned = first.strip_prefix("guest=").unwrap_or(first).trim();
        if cleaned.is_empty() {
            return None;
        }
        return Some(cleaned.to_string());
    }
    None
}

// ---------- /proc parsing voor I/O-tellers --------------------------------

/// Lees cumulatieve I/O voor een PID. Alle fouten (proces net afgesloten,
/// permission denied, namespace-quirks) worden stil opgevangen -> 0-tellers,
/// zodat de TUI nooit panic't door een verdwenen VM.
///
/// `ifaces` is de per-PID resolvde set VM-interfaces (zie `vm_ifaces_for_pid`);
/// alleen die tellen mee voor net-throughput, zodat elke VM enkel z'n eigen
/// verkeer krijgt toegerekend en niet de som van álle taps in de namespace.
fn read_io_counters(pid: u32, ifaces: &HashSet<String>) -> IoCounters {
    let mut c = IoCounters::default();
    read_proc_io(pid, &mut c);
    read_proc_net_dev(pid, ifaces, &mut c);
    c
}

/// Bepaal welke netwerk-interfaces bij één specifieke QEMU-PID horen, zodat
/// net-throughput per VM toegerekend wordt i.p.v. over de hele netwerk-
/// namespace (waar `/proc/[pid]/net/dev` álle taps van álle VM's toont).
///
/// Twee /proc-bronnen, gecombineerd:
///   1. tap file descriptors: `/proc/[pid]/fd/*` die naar het char-device van
///      een tap wijzen — `/dev/net/tun` (tun/tap-bridge) of `/dev/tapNN`
///      (macvtap). Beide draaien op de kernel tap-driver, die in
///      `/proc/[pid]/fdinfo/<fd>` een regel `iff:\t<naam>` met de exacte
///      interfacenaam schrijft. Dekt de libvirt-default (tap-fd doorgegeven).
///   2. expliciete `ifname=` in de QEMU-argv (`-netdev tap,...,ifname=vnetX`).
///
/// Vereist hetzelfde privilege als `/proc/[pid]/io` (root of de proces-owner);
/// alle leesfouten worden stil genegeerd. Levert dit niets op (exotische netns,
/// vhost-user, SR-IOV passthrough), dan blijft net-throughput 0 — conform de
/// README, i.p.v. verkeer verkeerd toe te rekenen.
fn vm_ifaces_for_pid(pid: u32, args: &[&str]) -> HashSet<String> {
    let mut ifaces = HashSet::new();

    // Bron 1: tap-fd's -> fdinfo "iff:".
    if let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) {
        for entry in entries.flatten() {
            let Ok(target) = fs::read_link(entry.path()) else { continue };
            if !target.to_str().is_some_and(is_tap_device_path) {
                continue;
            }
            let Some(fd) = entry.file_name().to_str().map(str::to_owned) else { continue };
            let Ok(info) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else { continue };
            for line in info.lines() {
                if let Some(name) = line.strip_prefix("iff:") {
                    let name = name.trim();
                    if !name.is_empty() {
                        ifaces.insert(name.to_string());
                    }
                }
            }
        }
    }

    // Bron 2: expliciete ifname= in de argv (aanvullend).
    ifaces.extend(extract_ifnames(args));

    ifaces
}

/// Herkent het char-device achter een tap-fd aan het pad dat
/// `readlink /proc/[pid]/fd/<n>` teruggeeft: `/dev/net/tun` (tun/tap) of
/// `/dev/tapNN` (macvtap). Ruim gehouden (basename begint met "tap") i.p.v.
/// een exacte string, want de definitieve gate is de `iff:`-regel in fdinfo —
/// een niet-tap device zonder die regel voegt niets toe.
fn is_tap_device_path(target: &str) -> bool {
    target == "/dev/net/tun"
        || target.rsplit('/').next().is_some_and(|base| base.starts_with("tap"))
}

/// Haal alle `ifname=<x>` waarden uit QEMU `-netdev`/`-net`-argv-tokens.
/// Tokens kunnen komma-gescheiden zijn: `tap,id=net0,ifname=vnet3,script=no`.
fn extract_ifnames(args: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for a in args {
        for part in a.split(',') {
            if let Some(v) = part.strip_prefix("ifname=") {
                let v = v.trim();
                if !v.is_empty() {
                    out.push(v.to_string());
                }
            }
        }
    }
    out
}

/// /proc/[pid]/io — read_bytes / write_bytes. Op RHEL 9 default beschikbaar
/// voor root wanneer CONFIG_TASK_IO_ACCOUNTING aan staat (= standaard).
fn read_proc_io(pid: u32, out: &mut IoCounters) {
    let path = format!("/proc/{pid}/io");
    let Ok(content) = fs::read_to_string(&path) else { return };
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            out.disk_read = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            out.disk_write = v.trim().parse().unwrap_or(0);
        }
    }
}

/// /proc/[pid]/net/dev — telt RX/TX bytes, maar alleen over de interfaces die
/// via `vm_ifaces_for_pid` aan deze specifieke VM zijn toegewezen. Zo krijgt
/// elke VM enkel z'n eigen tap-verkeer i.p.v. de som van alle taps in de
/// namespace. Een lege set (onherleidbaar) laat net-throughput op 0 staan.
fn read_proc_net_dev(pid: u32, ifaces: &HashSet<String>, out: &mut IoCounters) {
    if ifaces.is_empty() {
        return;
    }
    let Ok(content) = fs::read_to_string(format!("/proc/{pid}/net/dev")) else { return };
    let (rx, tx) = sum_net_dev(&content, ifaces);
    out.net_rx = rx;
    out.net_tx = tx;
}

/// Pure parser voor een `/proc/[pid]/net/dev`-bestand: sommeer RX- en TX-bytes
/// over uitsluitend de interfaces in `ifaces`. Apart gehouden zodat het zonder
/// echte /proc-toegang te testen is.
///
/// Formaat:
///   "Inter-|   Receive ...  |  Transmit ..."
///   "      |bytes packets errs drop fifo frame compressed multicast"
///   " eth0: 12345 ... 67890 ..."
/// RX bytes = veld 0, TX bytes = veld 8.
fn sum_net_dev(content: &str, ifaces: &HashSet<String>) -> (u64, u64) {
    let mut rx_total: u64 = 0;
    let mut tx_total: u64 = 0;
    for line in content.lines().skip(2) {
        let Some((iface, rest)) = line.split_once(':') else { continue };
        if !ifaces.contains(iface.trim()) {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() >= 9 {
            rx_total = rx_total.saturating_add(fields[0].parse().unwrap_or(0));
            tx_total = tx_total.saturating_add(fields[8].parse().unwrap_or(0));
        }
    }
    (rx_total, tx_total)
}

// ---------- vCPU / NUMA-detail --------------------------------------------
//
// Alles hier is pure /proc + /sys-tekst, net als de rest van dit bestand:
// geen QMP, geen libvirt. Vereist CAP_SYS_PTRACE naast CAP_DAC_READ_SEARCH
// voor numa_maps van een andere Linux-user (zie packaging/abyss-top.spec);
// de vCPU->pCPU->NUMA-node-mapping en schedstat-contentie werken al met
// alleen CAP_DAC_READ_SEARCH (empirisch geverifieerd: die velden zijn niet
// ptrace-mode-gated).

/// Bouwt fysieke-CPU -> NUMA-node uit /sys/devices/system/node/node*/cpulist.
/// Eenmalig aangeroepen in `App::new()`: host-topologie verandert niet
/// tijdens de levensduur van het proces. Een lege map (bv. geen NUMA-sysfs-
/// tak) is een geldige uitkomst — de detail-view toont dan gewoon geen
/// node-info i.p.v. te crashen.
fn build_cpu_to_node_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir("/sys/devices/system/node") else { return map };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(name) = fname.to_str() else { continue };
        let Some(node_str) = name.strip_prefix("node") else { continue };
        let Ok(node) = node_str.parse::<u32>() else { continue };
        let Ok(content) = fs::read_to_string(entry.path().join("cpulist")) else { continue };
        for cpu in parse_cpulist(content.trim()) {
            map.insert(cpu, node);
        }
    }
    map
}

/// Parseert een /sys `cpulist`-bestand ("0-3", "0-3,8-11", "5") naar de
/// losse CPU-nummers.
fn parse_cpulist(content: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in content.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            let (Ok(start), Ok(end)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) else {
                continue;
            };
            out.extend(start..=end);
        } else if let Ok(n) = part.parse::<u32>() {
            out.push(n);
        }
    }
    out
}

/// Herkent een QEMU-vCPU-threadnaam ("CPU 3/KVM"; ook /TCG /DUMMY /MSHV
/// afhankelijk van de accelerator, zie `strings` op qemu-kvm) en geeft de
/// vCPU-index terug. `None` voor elke andere threadnaam (I/O-thread,
/// main-loop, enz.) — die tellen niet mee in de detail-view.
fn parse_vcpu_index(comm: &str) -> Option<u32> {
    let rest = comm.trim().strip_prefix("CPU ")?;
    let (idx, accel) = rest.split_once('/')?;
    match accel {
        "KVM" | "TCG" | "DUMMY" | "MSHV" => idx.trim().parse().ok(),
        _ => None,
    }
}

/// Parseert veld 39 ("processor", huidige fysieke CPU) uit een
/// /proc/<pid>/task/<tid>/stat-regel. De comm (veld 2) staat tussen haakjes
/// en kan zelf spaties/haakjes bevatten (een vCPU-thread heet letterlijk
/// "CPU 0/KVM") — daarom zoeken we de LAATSTE ')' om de rest veilig af te
/// splitsen i.p.v. simpelweg op whitespace vanaf het begin.
fn parse_stat_processor_field(content: &str) -> Option<u32> {
    let close = content.rfind(')')?;
    let rest = content.get(close + 1..)?;
    // `rest` begint met " state ppid ...". state is hier token 0 (= veld 3
    // overall), dus veld 39 (processor) is token (39 - 3) = 36.
    rest.split_whitespace().nth(36)?.parse().ok()
}

/// Veld 2 (0-indexed 1) van /proc/<pid>/task/<tid>/schedstat: nanoseconden
/// die de thread runnable-maar-niet-lopend heeft gewacht — een host-CPU-
/// contentie-proxy ("noisy neighbor"-signaal), gevoed door dezelfde
/// `per_second()` als disk/net-tellers.
fn parse_schedstat_wait_ns(content: &str) -> Option<u64> {
    content.split_whitespace().nth(1)?.parse().ok()
}

/// Sommeert alle "N<node>=<pagina's>"-tokens over een heel
/// /proc/<pid>/numa_maps-bestand, ongeacht welke VMA-regel. De daadwerkelijke
/// guest-RAM-VMA('s) domineren de paginacount toch al ruimschoots t.o.v. de
/// kleine code/heap-VMA's van QEMU zelf, dus een globale som is al het juiste
/// signaal zonder te moeten bepalen "welke VMA is de guest-RAM".
fn sum_numa_maps_by_node(content: &str) -> HashMap<u32, u64> {
    let mut totals: HashMap<u32, u64> = HashMap::new();
    for line in content.lines() {
        for token in line.split_whitespace() {
            let Some(rest) = token.strip_prefix('N') else { continue };
            let Some((node_str, count_str)) = rest.split_once('=') else { continue };
            let Ok(node) = node_str.parse::<u32>() else { continue };
            let Ok(count) = count_str.parse::<u64>() else { continue };
            *totals.entry(node).or_insert(0) += count;
        }
    }
    totals
}

/// Thread-id's van een proces via /proc/<pid>/task/. Lege Vec bij elke
/// leesfout (proces net afgesloten, permission), zoals overal in dit bestand.
fn read_task_ids(pid: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/task")) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
        .collect()
}

/// Threadnaam via /proc/<pid>/task/<tid>/comm (géén haakjes-escaping nodig,
/// in tegenstelling tot het comm-veld in .../stat).
fn read_thread_comm(pid: u32, tid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Huidige fysieke CPU van een thread. Werkt al met alleen
/// CAP_DAC_READ_SEARCH (dit veld is niet ptrace-mode-gated).
fn read_thread_processor(pid: u32, tid: u32) -> Option<u32> {
    let content = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    parse_stat_processor_field(&content)
}

/// Schedstat-wachttijd (ns) van een thread. Werkt al met alleen
/// CAP_DAC_READ_SEARCH.
fn read_thread_schedstat_wait(pid: u32, tid: u32) -> Option<u64> {
    let content = fs::read_to_string(format!("/proc/{pid}/task/{tid}/schedstat")).ok()?;
    parse_schedstat_wait_ns(&content)
}

/// numa_maps van een heel proces, gesommeerd per node. Geeft de `io::Error`
/// door (i.p.v. te versimpelen tot een lege map) zodat de aanroeper
/// Permission denied (ontbrekende CAP_SYS_PTRACE bij een andere proces-
/// eigenaar) kan onderscheiden van "geen NUMA-info".
fn read_numa_maps(pid: u32) -> io::Result<HashMap<u32, u64>> {
    fs::read_to_string(format!("/proc/{pid}/numa_maps")).map(|c| sum_numa_maps_by_node(&c))
}

// ---------- Formatters ----------------------------------------------------

/// Bytes/s naar leesbare string met auto-schaling.
fn fmt_bps(bps: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bps as f64;
    if b < KB {
        format!("{:>6} B/s", bps)
    } else if b < MB {
        format!("{:>5.1} KB/s", b / KB)
    } else if b < GB {
        format!("{:>5.1} MB/s", b / MB)
    } else {
        format!("{:>5.1} GB/s", b / GB)
    }
}

/// Compacte "R / W" weergave voor een tabelcel.
fn fmt_rw(r: u64, w: u64) -> String {
    format!("{} / {}", fmt_bps(r), fmt_bps(w))
}

// ---------- UI ------------------------------------------------------------

mod ui {
    use super::*;

    pub fn render(f: &mut Frame, app: &mut App) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // titel
                Constraint::Min(5),    // tabel (+ evt. detail-view)
                Constraint::Length(3), // footer
            ])
            .split(f.size());

        render_title(f, chunks[0], app.vms.len());

        if app.expanded {
            let mid = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(chunks[1]);
            render_table(f, mid[0], &app.vms, app.sort_by, &mut app.table_state);
            render_detail(f, mid[1], app.detail.as_ref(), app.node_count);
        } else {
            render_table(f, chunks[1], &app.vms, app.sort_by, &mut app.table_state);
        }

        render_footer(f, chunks[2], app.host);
    }

    fn render_title(f: &mut Frame, area: Rect, vm_count: usize) {
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                "Abyss-Top",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" | KVM Hypervisor Monitor  "),
            Span::styled(
                format!("[{} VM{}]", vm_count, if vm_count == 1 { "" } else { "s" }),
                Style::default().fg(Color::Yellow),
            ),
        ]))
        .block(Block::default().borders(Borders::ALL).style(Style::default().fg(Color::DarkGray)));
        f.render_widget(title, area);
    }

    fn render_table(
        f: &mut Frame,
        area: Rect,
        vms: &[VmStats],
        sort_by: SortBy,
        table_state: &mut TableState,
    ) {
        let header_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);

        let header = Row::new(vec![
            Cell::from("PID"),
            Cell::from("VM Name"),
            Cell::from("CPU %"),
            Cell::from("MEM (MB)"),
            Cell::from("DISK R/W"),
            Cell::from("NET RX/TX"),
        ])
        .style(header_style)
        .height(1);

        let rows: Vec<Row> = if vms.is_empty() {
            vec![Row::new(vec![
                Cell::from("-"),
                Cell::from(Span::styled(
                    "No running QEMU/KVM guests detected",
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
            ])]
        } else {
            vms.iter()
                .map(|v| {
                    let cpu_color = match v.cpu_pct {
                        c if c >= 80.0 => Color::Red,
                        c if c >= 40.0 => Color::Yellow,
                        _ => Color::Green,
                    };
                    Row::new(vec![
                        Cell::from(v.pid.to_string()),
                        Cell::from(Span::styled(
                            v.name.clone(),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )),
                        Cell::from(Span::styled(
                            format!("{:>6.1}", v.cpu_pct),
                            Style::default().fg(cpu_color),
                        )),
                        Cell::from(format!("{:>8}", v.mem_mb)),
                        Cell::from(Span::styled(
                            fmt_rw(v.disk_read_bps, v.disk_write_bps),
                            Style::default().fg(Color::Magenta),
                        )),
                        Cell::from(Span::styled(
                            fmt_rw(v.net_rx_bps, v.net_tx_bps),
                            Style::default().fg(Color::LightBlue),
                        )),
                    ])
                })
                .collect()
        };

        // Vaste breedtes voor numerieke kolommen; VM Name vult de rest.
        let widths = [
            Constraint::Length(8),      // PID
            Constraint::Percentage(22), // VM Name
            Constraint::Length(8),      // CPU %
            Constraint::Length(10),     // MEM
            Constraint::Length(24),     // DISK R/W
            Constraint::Length(24),     // NET RX/TX
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Guests (sorted by {}) ", sort_by.label()))
                    .style(Style::default().fg(Color::DarkGray)),
            )
            .column_spacing(2)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");

        f.render_stateful_widget(table, area, table_state);
    }

    /// vCPU/NUMA-detail-view voor de geselecteerde VM: een tabel met per
    /// vCPU-thread de huidige fysieke CPU, NUMA-node en contentie ("wait %"),
    /// plus een regel met de geheugen-NUMA-locatie van de guest.
    fn render_detail(f: &mut Frame, area: Rect, detail: Option<&VmDetail>, node_count: usize) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" vCPU / NUMA detail ")
            .style(Style::default().fg(Color::DarkGray));

        let Some(detail) = detail else {
            let p = Paragraph::new(Line::from(Span::styled(
                "No data yet…",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            )))
            .block(block);
            f.render_widget(p, area);
            return;
        };

        let inner_area = block.inner(area);
        f.render_widget(block, area);

        let inner = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(inner_area);

        let header = Row::new(vec![
            Cell::from("TID"),
            Cell::from("vCPU"),
            Cell::from("pCPU"),
            Cell::from("NUMA"),
            Cell::from("wait %"),
        ])
        .style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
        .height(1);

        let rows: Vec<Row> = if detail.vcpus.is_empty() {
            vec![Row::new(vec![Cell::from(Span::styled(
                "No vCPU threads found (non-KVM accelerator, or guest just stopped)",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ))])]
        } else {
            detail
                .vcpus
                .iter()
                .map(|v| {
                    let wait_color = match v.wait_pct {
                        w if w >= 20.0 => Color::Red,
                        w if w >= 5.0 => Color::Yellow,
                        _ => Color::Green,
                    };
                    Row::new(vec![
                        Cell::from(v.tid.to_string()),
                        Cell::from(v.vcpu_index.to_string()),
                        Cell::from(v.cur_cpu.to_string()),
                        Cell::from(v.numa_node.map(|n| n.to_string()).unwrap_or_else(|| "-".into())),
                        Cell::from(Span::styled(
                            format!("{:>5.1}", v.wait_pct),
                            Style::default().fg(wait_color),
                        )),
                    ])
                })
                .collect()
        };

        let widths = [
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(8),
        ];
        let table = Table::new(rows, widths).header(header).column_spacing(2);
        f.render_widget(table, inner[0]);

        let mem_line = if node_count <= 1 {
            Line::from(Span::styled(
                "Memory locality: single-node host (N/A)",
                Style::default().fg(Color::DarkGray),
            ))
        } else if detail.denied {
            Line::from(Span::styled(
                "Memory locality: permission denied — needs CAP_SYS_PTRACE (reinstall/upgrade abyss-top)",
                Style::default().fg(Color::Red),
            ))
        } else if detail.mem_by_node.is_empty() {
            Line::from(Span::styled("Memory locality: no data", Style::default().fg(Color::DarkGray)))
        } else {
            let mut spans = vec![Span::styled("Memory locality: ", Style::default().fg(Color::Cyan))];
            for (i, (node, pct)) in detail.mem_by_node.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(format!("node{node} {pct:.0}%"), Style::default().fg(Color::White)));
            }
            let split = detail.mem_by_node.iter().filter(|(_, pct)| *pct > 5.0).count() > 1;
            if split {
                spans.push(Span::styled(
                    "  [split]",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            Line::from(spans)
        };
        f.render_widget(Paragraph::new(mem_line), inner[1]);
    }

    fn render_footer(f: &mut Frame, area: Rect, host: HostStats) {
        let mem_pct = if host.mem_total_mb == 0 {
            0.0
        } else {
            (host.mem_used_mb as f32 / host.mem_total_mb as f32) * 100.0
        };

        let line = Line::from(vec![
            Span::styled("Host ", Style::default().fg(Color::DarkGray)),
            Span::styled("CPU ", Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{:>5.1}%", host.cpu_pct),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled("RAM ", Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{}/{} MB ({:.1}%)", host.mem_used_mb, host.mem_total_mb, mem_pct),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "↑↓ select · [Enter] detail · sort [C]pu [D]isk [N]et [M]em · [Q]uit",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]);

        let footer = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .style(Style::default().fg(Color::DarkGray)),
        );
        f.render_widget(footer, area);
    }
}

// ---------- Terminal-lifecycle -------------------------------------------

type Term = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Term> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(terminal: &mut Term) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    Ok(())
}

/// Best-effort raw-herstel; gebruikt door de panic-hook waar we geen Terminal hebben.
fn force_restore_raw() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

fn install_panic_hook() {
    let default = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        force_restore_raw();
        default(info);
    }));
}

// ---------- Event-loop ----------------------------------------------------

fn run(terminal: &mut Term, app: &mut App) -> Result<()> {
    app.prime();
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui::render(f, app))?;

        // Blokkeer hoogstens het resterende tick-budget, refresh dan.
        let timeout = TICK_RATE.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code);
                }
            }
        }
        if last_tick.elapsed() >= TICK_RATE {
            app.refresh();
            last_tick = Instant::now();
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

// ---------- main ----------------------------------------------------------

fn main() -> Result<()> {
    install_panic_hook();

    let mut terminal = setup_terminal()?;
    let mut app = App::new();

    let result = run(&mut terminal, &mut app);

    // Altijd herstellen, ook bij Err.
    restore_terminal(&mut terminal).ok();

    result
}

// ---------- Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_guest_form() {
        let args = vec!["qemu-system-x86_64", "-name", "guest=cerberus-vm01,debug-threads=on"];
        assert_eq!(extract_vm_name(&args).as_deref(), Some("cerberus-vm01"));
    }

    #[test]
    fn parses_bare_form() {
        let args = vec!["qemu-system-x86_64", "-name", "abyssos-test"];
        assert_eq!(extract_vm_name(&args).as_deref(), Some("abyssos-test"));
    }

    #[test]
    fn parses_bare_with_trailing_opts() {
        let args = vec!["qemu-system-x86_64", "-name", "vm-42,debug-threads=on"];
        assert_eq!(extract_vm_name(&args).as_deref(), Some("vm-42"));
    }

    #[test]
    fn parses_double_dash() {
        let args = vec!["qemu-system-x86_64", "--name", "vm-alpha"];
        assert_eq!(extract_vm_name(&args).as_deref(), Some("vm-alpha"));
    }

    #[test]
    fn parses_equals_form() {
        let args = vec!["qemu-system-x86_64", "-name=vm-beta,debug-threads=on"];
        assert_eq!(extract_vm_name(&args).as_deref(), Some("vm-beta"));
    }

    #[test]
    fn missing_name_returns_none() {
        let args = vec!["qemu-system-x86_64", "-m", "4096"];
        assert!(extract_vm_name(&args).is_none());
    }

    #[test]
    fn detects_qemu_variants() {
        assert!(is_qemu_process("qemu-system-x86", None));
        assert!(is_qemu_process("qemu-kvm", None));
        assert!(is_qemu_process("kvm", None));
        assert!(is_qemu_process("xxx", Some("/usr/libexec/qemu-kvm")));
        assert!(!is_qemu_process("bash", Some("/usr/bin/bash")));
    }

    #[test]
    fn per_second_handles_counter_reset() {
        // cur < prev -> 0, geen onderloop.
        assert_eq!(per_second(100, 500, 1.0), 0);
        // Normale delta.
        assert_eq!(per_second(2048, 1024, 1.0), 1024);
        // Halve seconde.
        assert_eq!(per_second(2048, 1024, 0.5), 2048);
    }

    #[test]
    fn fmt_bps_scales() {
        assert!(fmt_bps(0).contains("B/s"));
        assert!(fmt_bps(2048).contains("KB/s"));
        assert!(fmt_bps(5 * 1024 * 1024).contains("MB/s"));
        assert!(fmt_bps(3u64 * 1024 * 1024 * 1024).contains("GB/s"));
    }

    #[test]
    fn extract_ifnames_from_netdev_args() {
        let args = vec![
            "qemu-system-x86_64",
            "-netdev",
            "tap,id=net0,ifname=vnet3,script=no,downscript=no",
            "-device",
            "virtio-net-pci,netdev=net0",
        ];
        assert_eq!(extract_ifnames(&args), vec!["vnet3".to_string()]);
    }

    #[test]
    fn extract_ifnames_none() {
        let args = vec!["qemu-system-x86_64", "-m", "4096"];
        assert!(extract_ifnames(&args).is_empty());
    }

    #[test]
    fn sum_net_dev_counts_only_selected_ifaces() {
        // Twee taps zichtbaar in de namespace; alleen vnet0 hoort bij deze VM.
        // host-NIC eth0 en lo mogen nooit meetellen.
        let content = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  1000     10    0    0    0     0          0         0     1000      10    0    0    0     0       0          0
 vnet0:  5000     50    0    0    0     0          0         0     7000      70    0    0    0     0       0          0
 vnet1:  9000     90    0    0    0     0          0         0     9000      90    0    0    0     0       0          0
  eth0: 12345    100    0    0    0     0          0         0    67890     200    0    0    0     0       0          0
";
        let ifaces: HashSet<String> = ["vnet0".to_string()].into_iter().collect();
        assert_eq!(sum_net_dev(content, &ifaces), (5000, 7000));
    }

    #[test]
    fn sum_net_dev_empty_set_is_zero() {
        let content = "hdr1\nhdr2\n vnet0: 5000 50 0 0 0 0 0 0 7000 70 0 0 0 0 0 0\n";
        assert_eq!(sum_net_dev(content, &HashSet::new()), (0, 0));
    }

    #[test]
    fn tap_device_path_detection() {
        assert!(is_tap_device_path("/dev/net/tun")); // tun/tap-bridge fd
        assert!(is_tap_device_path("/dev/tap5")); // macvtap fd
        assert!(is_tap_device_path("/dev/tap42"));
        assert!(!is_tap_device_path("/dev/null"));
        assert!(!is_tap_device_path("/dev/vhost-net"));
        assert!(!is_tap_device_path("/dev/kvm"));
    }

    #[test]
    fn sort_by_cpu_descending() {
        let mut vms = vec![
            VmStats {
                pid: 1, name: "a".into(), cpu_pct: 10.0, mem_mb: 0,
                disk_read_bps: 0, disk_write_bps: 0, net_rx_bps: 0, net_tx_bps: 0,
            },
            VmStats {
                pid: 2, name: "b".into(), cpu_pct: 50.0, mem_mb: 0,
                disk_read_bps: 0, disk_write_bps: 0, net_rx_bps: 0, net_tx_bps: 0,
            },
        ];
        sort_vms(&mut vms, SortBy::Cpu);
        assert_eq!(vms[0].pid, 2);
    }

    #[test]
    fn sort_by_disk_io_descending() {
        let mut vms = vec![
            VmStats {
                pid: 1, name: "a".into(), cpu_pct: 0.0, mem_mb: 0,
                disk_read_bps: 100, disk_write_bps: 100, net_rx_bps: 0, net_tx_bps: 0,
            },
            VmStats {
                pid: 2, name: "b".into(), cpu_pct: 0.0, mem_mb: 0,
                disk_read_bps: 10, disk_write_bps: 10, net_rx_bps: 0, net_tx_bps: 0,
            },
        ];
        sort_vms(&mut vms, SortBy::DiskIo);
        assert_eq!(vms[0].pid, 1);
    }

    #[test]
    fn parse_vcpu_index_matches_all_accelerators() {
        assert_eq!(parse_vcpu_index("CPU 3/KVM"), Some(3));
        assert_eq!(parse_vcpu_index("CPU 0/TCG"), Some(0));
        assert_eq!(parse_vcpu_index("CPU 12/DUMMY"), Some(12));
        assert_eq!(parse_vcpu_index("CPU 1/MSHV"), Some(1));
        assert_eq!(parse_vcpu_index("qemu-system-x86"), None);
        assert_eq!(parse_vcpu_index("CPU 3/BOGUS"), None);
    }

    #[test]
    fn parse_stat_processor_field_skips_parenthesized_comm() {
        // Echte /proc/<pid>/task/<tid>/stat-regel (pid 462181, dit is een
        // gewone `sleep`, maar de comm-escaping-truc geldt evengoed voor een
        // vCPU-thread wiens comm zelf "CPU 0/KVM" is, met een spatie erin).
        // veld 39 (processor) = 3.
        let stat = "462181 (sleep) S 462179 462179 462147 0 -1 4194560 187 0 0 0 0 0 0 0 20 0 1 0 39762025 3153920 416 18446744073709551615 93914003353600 93914003365697 140731401530928 0 0 0 0 0 0 1 0 0 17 3 0 0 0 0 0 9";
        assert_eq!(parse_stat_processor_field(stat), Some(3));
    }

    #[test]
    fn parse_stat_processor_field_handles_comm_with_space_and_slash() {
        // Zelfde echte regel als hierboven, maar met een vCPU-achtige comm
        // ("CPU 0/KVM", bevat zelf een spatie en een '/') en veld 39 gewijzigd
        // naar 7, om te bewijzen dat de laatste-')'-truc nodig EN voldoende is
        // ook als de comm zelf whitespace bevat.
        let stat = "100 (CPU 0/KVM) S 462179 462179 462147 0 -1 4194560 187 0 0 0 0 0 0 0 20 0 1 0 39762025 3153920 416 18446744073709551615 93914003353600 93914003365697 140731401530928 0 0 0 0 0 0 1 0 0 17 7 0 0 0 0 0 9";
        assert_eq!(parse_stat_processor_field(stat), Some(7));
    }

    #[test]
    fn parse_schedstat_wait_ns_reads_second_field() {
        // Echte /proc/<pid>/task/<tid>/schedstat-inhoud.
        assert_eq!(parse_schedstat_wait_ns("1532390 0 1"), Some(0));
        assert_eq!(parse_schedstat_wait_ns("998877 445566 12"), Some(445566));
    }

    #[test]
    fn parse_cpulist_handles_ranges_and_singles() {
        assert_eq!(parse_cpulist("0-3"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpulist("0-3,8-11"), vec![0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(parse_cpulist("5"), vec![5]);
        assert_eq!(parse_cpulist(""), Vec::<u32>::new());
    }

    #[test]
    fn sum_numa_maps_by_node_single_node_real_fixture() {
        // Echte /proc/<pid>/numa_maps-regel (dit host heeft maar 1 NUMA-node).
        let content = "556a0ecf1000 default file=/usr/bin/sleep mapped=2 mapmax=2 active=0 N0=2 kernelpagesize_kB=4\n\
                        556a0ecf3000 default file=/usr/bin/sleep mapped=3 mapmax=2 active=0 N0=3 kernelpagesize_kB=4\n";
        let totals = sum_numa_maps_by_node(content);
        assert_eq!(totals.get(&0), Some(&5));
        assert_eq!(totals.len(), 1);
    }

    #[test]
    fn sum_numa_maps_by_node_sums_across_multiple_nodes() {
        // Synthetisch: dit host is single-node, dus dit dekt het
        // multi-node-pad dat hier niet echt te reproduceren is.
        let content = "7f0000000000 interleave file=/mem N0=100 N1=50 kernelpagesize_kB=4\n\
                        7f0000200000 default anon=10 dirty=10 N1=10 kernelpagesize_kB=2048\n";
        let totals = sum_numa_maps_by_node(content);
        assert_eq!(totals.get(&0), Some(&100));
        assert_eq!(totals.get(&1), Some(&60));
    }
}
