//! Runtime configuration, resolved from environment variables.
//!
//! Everything has a sensible default so `nodelet` can run with zero config
//! against a kubeconfig-resolved cluster.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeKind {
    /// In-memory fake runtime: reports pods Running without a container engine.
    /// Lets you exercise/measure the full control loop with zero runtime overhead.
    Mock,
    /// Real containerd/CRI runtime (only available when built with `--features cri`).
    Cri,
}

/// Which address family(ies) the Service proxy (`svc.rs`) programs rules
/// for. Defaults to whatever the node actually has: both stacks if both
/// work, otherwise whichever one does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpFamily {
    V4,
    V6,
    Dual,
}

/// Load-balancing algorithm for Services without `sessionAffinity: ClientIP`
/// set (that field always forces source-hash — see `svc.rs::lb_expr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LbMethod {
    Random,
    RoundRobin,
    SourceHash,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub node_name: String,
    pub runtime: RuntimeKind,
    #[cfg_attr(not(feature = "cri"), allow(dead_code))]
    pub cri_endpoint: String,
    /// Lease renewal interval — cheap liveness signal (default 10s).
    pub heartbeat: Duration,
    /// Node-status push interval — heavier, infrequent (default 60s).
    pub status_interval: Duration,
    pub cpu_cores: u64,
    pub memory_bytes: u64,
    pub max_pods: u64,
    pub labels: BTreeMap<String, String>,
    /// Program ClusterIP/NodePort nftables rules (see `svc.rs`). Defaults to
    /// on for the `cri` runtime (where pods have real IPs worth routing to)
    /// and off for `mock` (nothing real to route to).
    pub service_proxy: bool,
    pub ip_family: IpFamily,
    pub lb_method: LbMethod,
    /// MemoryPressure condition fires when `/proc/meminfo`'s MemAvailable
    /// drops below this (default 100Mi, matching kubelet's default eviction
    /// threshold `memory.available<100Mi`).
    pub memory_pressure_threshold_bytes: u64,
    /// Filesystem path DiskPressure is measured against (default: nodelet's
    /// own state dir, since that's where images/volumes/logs actually land).
    pub disk_path: String,
    /// DiskPressure condition fires when available space on `disk_path`
    /// drops below this percent (default 10, matching kubelet's default
    /// `nodefs.available<10%`).
    pub disk_pressure_percent: u8,
    /// PIDPressure condition fires when available PIDs (`pid_max` minus
    /// running processes) drops below this percent (default 10).
    pub pid_pressure_percent: u8,
    /// How often orphaned-sandbox and unreferenced-image GC runs (cri
    /// runtime only; no-op on mock). Coarse on purpose — not a poll loop
    /// for pod state, just periodic housekeeping.
    pub gc_interval: Duration,
    /// Cluster DNS server IPs, injected into a pod's `resolv.conf` when its
    /// `dnsPolicy` is `ClusterFirst` (the default) — real kubelet's
    /// `--cluster-dns`. Empty means "no cluster DNS configured", in which
    /// case pods fall back to the host's own resolv.conf.
    pub cluster_dns: Vec<String>,
    /// Real kubelet's `--cluster-domain` — the base domain used to build a
    /// ClusterFirst pod's DNS search list.
    pub cluster_domain: String,
    /// How often node-pressure eviction re-checks MemoryPressure/DiskPressure
    /// and, if either is active, evicts one eligible pod (see `eviction.rs`).
    pub eviction_check_interval: Duration,
    /// Real kubelet's `--container-log-max-size`: a running container's log
    /// file is rotated once it exceeds this many bytes.
    pub container_log_max_size_bytes: u64,
    /// Real kubelet's `--container-log-max-files`: how many rotated log
    /// files (including the active one) survive per container.
    pub container_log_max_files: u32,
    /// How often the log-rotation check runs.
    pub log_rotate_interval: Duration,
    /// Real kubelet's `staticPodPath` — a directory of Pod manifests run
    /// directly on this node (no scheduler), mirrored into the apiserver as
    /// read-only Pod objects. `None` (unset) disables the feature entirely,
    /// matching upstream: it's likewise optional there.
    pub static_pod_path: Option<String>,
    /// How often the static pod manifest directory is rescanned.
    pub static_pod_sync_interval: Duration,
    /// Whether the kubelet-style HTTP(S) server (`server.rs`, `cri`
    /// feature only — containerLogs/exec/attach/portForward) runs at all.
    /// On by default when built with `--features cri`, same as every
    /// other CRI-only background loop.
    pub server_enabled: bool,
    /// Real kubelet's default port (10250) for the same server.
    pub server_port: u16,
    /// Where the server's self-signed TLS cert/key are generated/cached.
    pub server_cert_dir: String,
    /// Real kubelet's `ShutdownGracePeriod` — total time budget to terminate
    /// pods after systemd-logind signals an imminent shutdown, before
    /// releasing the inhibitor lock and letting shutdown proceed. `0`
    /// (default) disables graceful node shutdown entirely, matching
    /// upstream: it's opt-in there too (`shutdown.rs` is a no-op loop when
    /// this is `0`).
    pub shutdown_grace_period_seconds: u64,
    /// Real kubelet's `ShutdownGracePeriodCriticalPods` — a sub-budget of
    /// the above reserved for `system-node-critical`/`system-cluster-critical`
    /// pods, which are terminated last so ordinary pods get first crack at
    /// a clean exit. Must be `<= shutdown_grace_period_seconds`; clamped if not.
    pub shutdown_grace_period_critical_seconds: u64,
    /// Real kubelet's `--system-reserved=cpu=...`: CPU reserved for
    /// non-Kubernetes host processes (sshd, systemd, etc.), subtracted from
    /// `Node.status.allocatable` (but not `capacity`). Default `0`, matching
    /// upstream: reservation is opt-in there too.
    pub system_reserved_cpu_millicores: u64,
    /// Same, for memory bytes.
    pub system_reserved_memory_bytes: u64,
    /// Real kubelet's `--kube-reserved=cpu=...`: CPU reserved for
    /// Kubernetes system daemons themselves (kubelet/nodelet, the container
    /// runtime). Default `0`.
    pub kube_reserved_cpu_millicores: u64,
    /// Same, for memory bytes.
    pub kube_reserved_memory_bytes: u64,
    /// Where the host's cgroup v2 unified hierarchy is mounted — used only
    /// to create/cap the top-level `kubepods` cgroup at
    /// `Node.status.allocatable` (`cri` only; see `cgroup.rs`). Configurable
    /// mainly so tests can point this at a throwaway directory instead of
    /// the real `/sys/fs/cgroup`.
    pub cgroup_fs_root: String,
    /// Statically-configured CSI driver name -> Node-service unix socket
    /// endpoint (`cri` only; see `runtime/csi.rs`), e.g.
    /// `NODELET_CSI_DRIVERS=hostpath.csi.k8s.io=unix:///var/lib/nodelet/csi-sockets/hostpath.sock`.
    /// Empty by default — `PersistentVolumeClaim` volumes are skipped with
    /// a warning (same treatment any other unresolvable volume gets) until
    /// at least one driver is configured. Real kubelet discovers CSI
    /// sockets dynamically via a plugin-registration protocol nodelet
    /// doesn't implement (see the module doc comment on `runtime/csi.rs`);
    /// this is the deliberately simpler static alternative.
    pub csi_drivers: BTreeMap<String, String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let node_name = env_or("NODELET_NODE_NAME", detect_hostname);

        let runtime = match std::env::var("NODELET_RUNTIME").as_deref() {
            Ok("cri") => RuntimeKind::Cri,
            Ok("mock") | Err(_) => RuntimeKind::Mock,
            Ok(other) => anyhow::bail!("unknown NODELET_RUNTIME '{other}' (want 'mock' or 'cri')"),
        };

        let cri_endpoint = std::env::var("NODELET_CRI_ENDPOINT")
            .unwrap_or_else(|_| "unix:///run/containerd/containerd.sock".to_string());

        let heartbeat = Duration::from_secs(env_u64("NODELET_HEARTBEAT_SECS", 10)?);
        let status_interval = Duration::from_secs(env_u64("NODELET_STATUS_SECS", 60)?);

        let cpu_cores = match std::env::var("NODELET_CPU") {
            Ok(v) => v.parse().context("NODELET_CPU must be an integer")?,
            Err(_) => detect_cpu_cores(),
        };

        let memory_bytes = match std::env::var("NODELET_MEMORY_BYTES") {
            Ok(v) => v.parse().context("NODELET_MEMORY_BYTES must be an integer")?,
            Err(_) => detect_memory_bytes(),
        };

        let max_pods = env_u64("NODELET_MAX_PODS", 110)?;

        let mut labels = BTreeMap::new();
        if let Ok(raw) = std::env::var("NODELET_LABELS") {
            for pair in raw.split(',').filter(|s| !s.trim().is_empty()) {
                let (k, v) = pair
                    .split_once('=')
                    .with_context(|| format!("bad label '{pair}', expected k=v"))?;
                labels.insert(k.trim().to_string(), v.trim().to_string());
            }
        }

        let service_proxy = match std::env::var("NODELET_SERVICE_PROXY").as_deref() {
            Ok("true") => true,
            Ok("false") => false,
            Ok(other) => anyhow::bail!("unknown NODELET_SERVICE_PROXY '{other}' (want 'true' or 'false')"),
            Err(_) => matches!(runtime, RuntimeKind::Cri),
        };

        let ip_family = match std::env::var("NODELET_IP_FAMILY").as_deref() {
            Ok("ipv4") => IpFamily::V4,
            Ok("ipv6") => IpFamily::V6,
            Ok("dual") => IpFamily::Dual,
            Ok("auto") | Err(_) => detect_ip_family(),
            Ok(other) => anyhow::bail!("unknown NODELET_IP_FAMILY '{other}' (want 'auto', 'ipv4', 'ipv6', or 'dual')"),
        };

        let lb_method = match std::env::var("NODELET_LB_METHOD").as_deref() {
            Ok("random") | Err(_) => LbMethod::Random,
            Ok("round-robin") => LbMethod::RoundRobin,
            Ok("source-hash") => LbMethod::SourceHash,
            Ok(other) => anyhow::bail!(
                "unknown NODELET_LB_METHOD '{other}' (want 'random', 'round-robin', or 'source-hash')"
            ),
        };

        let memory_pressure_threshold_bytes =
            env_u64("NODELET_MEMORY_PRESSURE_THRESHOLD_BYTES", 100 * 1024 * 1024)?;
        let disk_path = std::env::var("NODELET_DISK_PATH").unwrap_or_else(|_| "/var/lib/nodelet".to_string());
        let disk_pressure_percent = match std::env::var("NODELET_DISK_PRESSURE_PERCENT") {
            Ok(v) => v.parse().context("NODELET_DISK_PRESSURE_PERCENT must be an integer 0-100")?,
            Err(_) => 10u8,
        };
        let pid_pressure_percent = match std::env::var("NODELET_PID_PRESSURE_PERCENT") {
            Ok(v) => v.parse().context("NODELET_PID_PRESSURE_PERCENT must be an integer 0-100")?,
            Err(_) => 10u8,
        };
        let container_log_max_size_bytes =
            env_u64("NODELET_CONTAINER_LOG_MAX_SIZE_BYTES", 10 * 1024 * 1024)?;
        let container_log_max_files = env_u64("NODELET_CONTAINER_LOG_MAX_FILES", 5)? as u32;
        let log_rotate_interval = Duration::from_secs(env_u64("NODELET_LOG_ROTATE_INTERVAL_SECS", 10)?);
        let static_pod_path = std::env::var("NODELET_STATIC_POD_PATH").ok().filter(|s| !s.is_empty());
        let static_pod_sync_interval = Duration::from_secs(env_u64("NODELET_STATIC_POD_SYNC_SECS", 20)?);

        let server_enabled = match std::env::var("NODELET_SERVER_ENABLED").as_deref() {
            Ok("true") => true,
            Ok("false") => false,
            Ok(other) => anyhow::bail!("unknown NODELET_SERVER_ENABLED '{other}' (want 'true' or 'false')"),
            Err(_) => matches!(runtime, RuntimeKind::Cri),
        };
        let server_port = env_u64("NODELET_SERVER_PORT", 10250)? as u16;
        let server_cert_dir =
            std::env::var("NODELET_SERVER_CERT_DIR").unwrap_or_else(|_| "/var/lib/nodelet/pki".to_string());
        let gc_interval = Duration::from_secs(env_u64("NODELET_GC_INTERVAL_SECS", 300)?);

        let cluster_dns: Vec<String> = std::env::var("NODELET_CLUSTER_DNS")
            .ok()
            .map(|raw| raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let cluster_domain =
            std::env::var("NODELET_CLUSTER_DOMAIN").unwrap_or_else(|_| "cluster.local".to_string());
        let eviction_check_interval = Duration::from_secs(env_u64("NODELET_EVICTION_CHECK_SECS", 10)?);

        let shutdown_grace_period_seconds = env_u64("NODELET_SHUTDOWN_GRACE_PERIOD_SECS", 0)?;
        let shutdown_grace_period_critical_seconds =
            env_u64("NODELET_SHUTDOWN_GRACE_PERIOD_CRITICAL_SECS", 0)?.min(shutdown_grace_period_seconds);

        let system_reserved_cpu_millicores = env_u64("NODELET_SYSTEM_RESERVED_CPU_MILLICORES", 0)?;
        let system_reserved_memory_bytes = env_u64("NODELET_SYSTEM_RESERVED_MEMORY_BYTES", 0)?;
        let kube_reserved_cpu_millicores = env_u64("NODELET_KUBE_RESERVED_CPU_MILLICORES", 0)?;
        let kube_reserved_memory_bytes = env_u64("NODELET_KUBE_RESERVED_MEMORY_BYTES", 0)?;
        let cgroup_fs_root = std::env::var("NODELET_CGROUP_FS_ROOT").unwrap_or_else(|_| "/sys/fs/cgroup".to_string());

        let mut csi_drivers = BTreeMap::new();
        if let Ok(raw) = std::env::var("NODELET_CSI_DRIVERS") {
            for pair in raw.split(',').filter(|s| !s.trim().is_empty()) {
                let (name, endpoint) = pair
                    .split_once('=')
                    .with_context(|| format!("bad NODELET_CSI_DRIVERS entry '{pair}', expected name=endpoint"))?;
                csi_drivers.insert(name.trim().to_string(), endpoint.trim().to_string());
            }
        }

        Ok(Self {
            node_name,
            runtime,
            cri_endpoint,
            heartbeat,
            status_interval,
            cpu_cores,
            memory_bytes,
            max_pods,
            labels,
            service_proxy,
            ip_family,
            lb_method,
            memory_pressure_threshold_bytes,
            disk_path,
            disk_pressure_percent,
            pid_pressure_percent,
            container_log_max_size_bytes,
            container_log_max_files,
            log_rotate_interval,
            static_pod_path,
            static_pod_sync_interval,
            server_enabled,
            server_port,
            server_cert_dir,
            gc_interval,
            cluster_dns,
            cluster_domain,
            eviction_check_interval,
            shutdown_grace_period_seconds,
            shutdown_grace_period_critical_seconds,
            system_reserved_cpu_millicores,
            system_reserved_memory_bytes,
            kube_reserved_cpu_millicores,
            kube_reserved_memory_bytes,
            cgroup_fs_root,
            csi_drivers,
        })
    }
}

fn env_or(key: &str, default: impl FnOnce() -> String) -> String {
    std::env::var(key).ok().filter(|s| !s.is_empty()).unwrap_or_else(default)
}

fn env_u64(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(v) => v.parse().with_context(|| format!("{key} must be an integer")),
        Err(_) => Ok(default),
    }
}

fn detect_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "nodelet".to_string())
}

/// Binding a socket in each family is a direct, distro-agnostic test of
/// "can this process actually use this stack" — more reliable than parsing
/// `/proc/sys/net/ipv6/...`, which varies by how IPv6 was disabled (kernel
/// cmdline, sysctl, or just never configured).
/// Whether this host has an actual route for the given family — not just a
/// working socket API for it. A bare `bind()` only proves the kernel has
/// that address family compiled in and enabled, which is true on nearly
/// every modern Linux kernel regardless of whether there's any real
/// connectivity; that produced a real false positive (a machine with IPv6
/// support but no default v6 route was detected as dual-stack, and the
/// flannel CNI daemon this feeds into crash-looped forever trying to find a
/// v6 interface that didn't exist). `connect()` on a UDP socket sends no
/// packets — it's a local routing-table lookup for the given destination —
/// so this works fully offline and doesn't need the probe address itself to
/// be reachable, only *routable*.
fn has_route(probe_addr: &str, bind_addr: &str) -> bool {
    let Ok(sock) = std::net::UdpSocket::bind(bind_addr) else { return false };
    sock.connect(probe_addr).is_ok()
}

fn detect_ip_family() -> IpFamily {
    let v4 = has_route("8.8.8.8:53", "0.0.0.0:0");
    let v6 = has_route("[2001:4860:4860::8888]:53", "[::]:0");
    match (v4, v6) {
        (true, true) => IpFamily::Dual,
        (true, false) => IpFamily::V4,
        (false, true) => IpFamily::V6,
        (false, false) => IpFamily::V4, // shouldn't happen; fall back to the common case
    }
}

fn detect_cpu_cores() -> u64 {
    std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1)
}

fn detect_memory_bytes() -> u64 {
    // Parse MemTotal (kB) from /proc/meminfo.
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
                    return kb * 1024;
                }
            }
        }
    }
    2 * 1024 * 1024 * 1024 // 2 GiB fallback
}
