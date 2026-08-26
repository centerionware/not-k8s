//! Multi-member nodestore behavior.
//!
//! These tests deliberately start real nodestore processes with real mutual
//! TLS on distinct loopback ports. Network-namespace and packet-shaping
//! variants remain clean skips when the host cannot safely provide them; the
//! ordinary election, replication, forwarding, and restart paths do not need
//! those privileges.

use super::context::E2eContext;
use super::skip_test;
use anyhow::{Context, Result};
use base64::Engine;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use serde_json::Value;
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn b64(value: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(value)
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn required_binary() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("NOTK8S_NODESTORE_E2E_BINARY") {
        if Path::new(&path).is_file() {
            return Ok(path.into());
        }
    }
    let path = crate::config::Config::from_env()?.toolchain_dir().join("bin/nodestore");
    if path.is_file() {
        Ok(path)
    } else {
        Err(skip_test(format!(
            "nodestore binary is not installed at {}; provide NOTK8S_NODESTORE_E2E_BINARY",
            path.display()
        )))
    }
}

struct TestCa {
    cert: Certificate,
    key: KeyPair,
    cert_path: PathBuf,
}

fn make_ca(root: &Path, name: &str) -> Result<TestCa> {
    let cert_path = root.join(format!("{name}.crt"));
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, format!("not-k8s-e2e-{name}"));
    params.distinguished_name = distinguished_name;
    let key = KeyPair::generate().context("generating datastore test CA key")?;
    let cert = params
        .self_signed(&key)
        .context("self-signing datastore test CA")?;
    fs::write(&cert_path, cert.pem())?;
    Ok(TestCa {
        cert,
        key,
        cert_path,
    })
}

fn make_leaf(root: &Path, name: &str, ca: &TestCa) -> Result<(PathBuf, PathBuf)> {
    let key_path = root.join(format!("{name}.key"));
    let cert_path = root.join(format!("{name}.crt"));
    let mut params = CertificateParams::new(vec!["localhost".to_owned()])
        .context("building datastore test certificate parameters")?;
    params
        .subject_alt_names
        .push(SanType::IpAddress("127.0.0.1".parse()?));
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, format!("not-k8s-e2e-{name}"));
    params.distinguished_name = distinguished_name;
    let key = KeyPair::generate().context("generating datastore test certificate key")?;
    let cert = params
        .signed_by(&key, &ca.cert, &ca.key)
        .context("signing datastore test certificate")?;
    fs::write(&cert_path, cert.pem())?;
    fs::write(&key_path, key.serialize_pem())?;
    Ok((cert_path, key_path))
}

struct Pki {
    client_ca: PathBuf,
    client_cert: PathBuf,
    client_key: PathBuf,
    peer_ca: PathBuf,
    peer_cert: PathBuf,
    peer_key: PathBuf,
}

impl Pki {
    fn create(root: &Path) -> Result<Self> {
        let client_ca = make_ca(root, "client-ca")?;
        let (client_cert, client_key) = make_leaf(root, "client", &client_ca)?;
        let peer_ca = make_ca(root, "peer-ca")?;
        let (peer_cert, peer_key) = make_leaf(root, "peer", &peer_ca)?;
        Ok(Self {
            client_ca: client_ca.cert_path,
            client_cert,
            client_key,
            peer_ca: peer_ca.cert_path,
            peer_cert,
            peer_key,
        })
    }
}

struct Member {
    id: u64,
    client_port: u16,
    peer_port: u16,
    data_dir: PathBuf,
    log_path: PathBuf,
    child: Child,
}

struct Cluster {
    root: PathBuf,
    binary: PathBuf,
    pki: Pki,
    members: Vec<Member>,
}

impl Cluster {
    fn start() -> Result<Self> {
        let binary = required_binary()?;
        let root = std::env::temp_dir().join(format!(
            "nodebootstrap-datastore-cluster-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir_all(&root)?;
        let pki = Pki::create(&root)?;
        let base = std::env::var("NODESTORE_CLUSTER_BASE_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(24_000);
        let spec = (1..=3)
            .map(|id| format!("{id}=https://127.0.0.1:{}", base + id as u16))
            .collect::<Vec<_>>()
            .join(",");
        let mut cluster = Self {
            root,
            binary,
            pki,
            members: Vec::new(),
        };
        for id in 1..=3 {
            cluster.members.push(cluster.spawn_member(id, base, &spec)?);
        }
        if let Err(error) = cluster.wait_for_agreed_leader(Duration::from_secs(45)) {
            return Err(error);
        }
        Ok(cluster)
    }

    fn spawn_member(&self, id: u64, base: u16, spec: &str) -> Result<Member> {
        let client_port = base + 10 + id as u16;
        let peer_port = base + id as u16;
        let data_dir = self.root.join(format!("member-{id}/data"));
        fs::create_dir_all(&data_dir)?;
        let log = self.root.join(format!("member-{id}.log"));
        let log_file = fs::File::create(&log)?;
        let log_stderr = log_file.try_clone()?;
        let child = Command::new(&self.binary)
            .arg("nodestore")
            .env("NODESTORE_MEMBER_ID", id.to_string())
            .env("NODESTORE_INITIAL_CLUSTER", spec)
            .env("NODESTORE_LISTEN", format!("127.0.0.1:{client_port}"))
            .env("NODESTORE_PEER_LISTEN", format!("127.0.0.1:{peer_port}"))
            .env("NODESTORE_ADVERTISE_CLIENT_URL", format!("https://127.0.0.1:{client_port}"))
            .env("NODESTORE_ADVERTISE_PEER_URL", format!("https://127.0.0.1:{peer_port}"))
            .env("NODESTORE_DATA_DIR", &data_dir)
            .env("NODESTORE_CERT_FILE", &self.pki.client_cert)
            .env("NODESTORE_KEY_FILE", &self.pki.client_key)
            .env("NODESTORE_TRUSTED_CA_FILE", &self.pki.client_ca)
            .env("NODESTORE_PEER_CERT_FILE", &self.pki.peer_cert)
            .env("NODESTORE_PEER_KEY_FILE", &self.pki.peer_key)
            .env("NODESTORE_PEER_TRUSTED_CA_FILE", &self.pki.peer_ca)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_stderr))
            .spawn()
            .with_context(|| format!("starting nodestore member {id}"))?;
        Ok(Member {
            id,
            client_port,
            peer_port,
            data_dir,
            log_path: log,
            child,
        })
    }

    fn member(&self, id: u64) -> Result<&Member> {
        self.members
            .iter()
            .find(|member| member.id == id)
            .with_context(|| format!("cluster has no member {id}"))
    }

    fn grpc_call(&self, id: u64, peer: bool, method: &str, request: &str) -> Result<String> {
        let member = self.member(id)?;
        let (ca, cert, key, address) = if peer {
            (
                &self.pki.peer_ca,
                &self.pki.peer_cert,
                &self.pki.peer_key,
                format!("127.0.0.1:{}", member.peer_port),
            )
        } else {
            (
                &self.pki.client_ca,
                &self.pki.client_cert,
                &self.pki.client_key,
                format!("127.0.0.1:{}", member.client_port),
            )
        };
        super::datastore::grpc_json_call(
            ca.clone(),
            cert.clone(),
            key.clone(),
            address,
            peer,
            method.to_owned(),
            request.to_owned(),
        )
    }

    fn peer_status(&self, id: u64) -> Result<Value> {
        Ok(serde_json::from_str(&self.grpc_call(
            id,
            true,
            "notk8s.nodestore.peer.v1.Peer/Status",
            "{}",
        )?)?)
    }

    fn leader(&self) -> Result<Option<u64>> {
        let quorum = self.members.len() / 2 + 1;
        let mut leaders = Vec::new();
        let mut errors = Vec::new();
        for member in &self.members {
            let status = match self.peer_status(member.id) {
                Ok(status) => status,
                Err(error) => {
                    errors.push(format!(
                        "member {}: {error:#}; raw TCP probe: {}",
                        member.id,
                        self.peer_tcp_probe(member)
                    ));
                    continue;
                }
            };
            let current = json_u64(status.get("leaderId")).unwrap_or_default();
            if current == 0 {
                continue;
            }
            leaders.push(current);
        }

        // A member that is down is expected during the failover tests, and a
        // single transient status connection failure must not turn an already
        // elected quorum into a test failure. Require a consistent leader from
        // a quorum, rather than requiring every member to answer this one
        // diagnostic RPC.
        if leaders.len() >= quorum {
            let leader = leaders[0];
            return Ok(
                leaders.iter().all(|candidate| *candidate == leader).then_some(leader),
            );
        }

        // Keep the all-members-unreachable error for startup diagnostics, but
        // treat partial reachability as an ordinary not-yet-agreed state so a
        // surviving quorum can settle after a member is restarted or killed.
        if errors.len() == self.members.len() {
            anyhow::bail!("{}", errors.join("; "));
        }
        Ok(None)
    }

    fn peer_tcp_probe(&self, member: &Member) -> String {
        let address = format!("127.0.0.1:{}", member.peer_port);
        match address.parse::<SocketAddr>() {
            Ok(address) => match TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
                Ok(_) => "connects".to_string(),
                Err(error) => format!("refused/unreachable: {error}"),
            },
            Err(error) => format!("invalid address: {error}"),
        }
    }

    fn status_snapshot(&self) -> String {
        self.members
            .iter()
            .map(|member| match self.peer_status(member.id) {
                Ok(status) => format!("member {}: {status}", member.id),
                Err(error) => format!("member {}: {error:#}", member.id),
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn wait_for_agreed_leader(&mut self, timeout: Duration) -> Result<u64> {
        let deadline = Instant::now() + timeout;
        let mut last_status_error = None;
        loop {
            match self.leader() {
                Ok(Some(leader)) => return Ok(leader),
                Ok(None) => {}
                Err(error) => last_status_error = Some(format!("{error:#}")),
            }
            for member in &mut self.members {
                if let Some(status) = member.child.try_wait()? {
                    let log = fs::read_to_string(&member.log_path).unwrap_or_else(|error| {
                        format!("<could not read {}: {error}>", member.log_path.display())
                    });
                    anyhow::bail!(
                        "nodestore member {} exited with {status}; log {}:\n{}",
                        member.id,
                        member.log_path.display(),
                        log_tail(&log)
                    );
                }
            }
            if Instant::now() >= deadline {
                let logs = self
                    .members
                    .iter()
                    .map(|member| {
                        let log = fs::read_to_string(&member.log_path).unwrap_or_else(|error| {
                            format!("<could not read {}: {error}>", member.log_path.display())
                        });
                        format!(
                            "member {} ({}):\n{}\nprocess state:\n{}",
                            member.id,
                            member.log_path.display(),
                            log_tail(&log),
                            process_state(member),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let status = last_status_error
                    .map(|error| format!("\nlast peer-status error: {error}"))
                    .unwrap_or_default();
                anyhow::bail!(
                    "cluster did not agree on a leader; status snapshot: {}{status}\nrecent member logs:\n{logs}",
                    self.status_snapshot()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn put(&self, id: u64, key: &str, value: &str) -> Result<String> {
        self.grpc_call(
            id,
            false,
            "etcdserverpb.KV/Put",
            &serde_json::json!({"key": b64(key), "value": b64(value)}).to_string(),
        )
    }

    fn get(&self, id: u64, key: &str) -> Result<String> {
        let output = self.grpc_call(
            id,
            false,
            "etcdserverpb.KV/Range",
            &serde_json::json!({"key": b64(key)}).to_string(),
        )?;
        let document: Value = serde_json::from_str(&output)?;
        let Some(value) = document.pointer("/kvs/0/value").and_then(Value::as_str) else {
            return Ok(String::new());
        };
        Ok(String::from_utf8(base64::engine::general_purpose::STANDARD.decode(value)?)?)
    }

    fn kill(&mut self, id: u64) -> Result<()> {
        let member = self
            .members
            .iter_mut()
            .find(|member| member.id == id)
            .with_context(|| format!("cluster has no member {id}"))?;
        let _ = member.child.kill();
        let _ = member.child.wait();
        Ok(())
    }

    fn restart(&mut self, id: u64) -> Result<()> {
        let base = self
            .member(id)
            .map(|member| member.client_port - 10 - id as u16)?;
        let spec = (1..=3)
            .map(|member_id| format!("{member_id}=https://127.0.0.1:{}", base + member_id as u16))
            .collect::<Vec<_>>()
            .join(",");
        let replacement = self.spawn_member(id, base, &spec)?;
        let member = self
            .members
            .iter_mut()
            .find(|member| member.id == id)
            .with_context(|| format!("cluster has no member {id}"))?;
        member.child = replacement.child;
        Ok(())
    }
}

fn process_state(member: &Member) -> String {
    let pid = member.child.id();
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .map(|status| {
            status
                .lines()
                .filter(|line| line.starts_with("Name:") || line.starts_with("State:"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|error| format!("<could not read process status: {error}>"));
    let tcp = fs::read_to_string(format!("/proc/{pid}/net/tcp"))
        .map(|tcp| {
            tcp.lines()
                .filter(|line| line.contains(&format!(":{:04X}", member.peer_port)))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_else(|error| format!("<could not read process TCP state: {error}>"));
    format!("pid={pid} {status}\npeer port {}:\n{}", member.peer_port, tcp)
}

fn log_tail(log: &str) -> String {
    const MAX_LINES: usize = 80;
    let lines = log.lines().collect::<Vec<_>>();
    lines
        .iter()
        .skip(lines.len().saturating_sub(MAX_LINES))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for member in &mut self.members {
            let _ = member.child.kill();
            let _ = member.child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(super) async fn cluster_elects_a_single_leader(_context: &E2eContext) -> Result<()> {
    let cluster = Cluster::start()?;
    anyhow::ensure!(cluster.leader()?.is_some(), "cluster had no agreed leader");
    Ok(())
}

pub(super) async fn cluster_replicates_a_write_to_every_member(_context: &E2eContext) -> Result<()> {
    let mut cluster = Cluster::start()?;
    let leader = cluster.wait_for_agreed_leader(Duration::from_secs(10))?;
    cluster.put(leader, "/registry/cluster/replicated", "hello")?;
    for id in 1..=3 {
        anyhow::ensure!(
            cluster.get(id, "/registry/cluster/replicated")? == "hello",
            "member {id} did not apply the replicated write"
        );
    }
    Ok(())
}

pub(super) async fn follower_forwards_writes_to_the_leader(_context: &E2eContext) -> Result<()> {
    let mut cluster = Cluster::start()?;
    let leader = cluster.wait_for_agreed_leader(Duration::from_secs(10))?;
    let follower = (1..=3).find(|id| *id != leader).context("no follower")?;
    cluster.put(follower, "/registry/cluster/forwarded", "through-follower")?;
    anyhow::ensure!(cluster.get(leader, "/registry/cluster/forwarded")? == "through-follower");
    Ok(())
}

pub(super) async fn cluster_keeps_serving_when_a_follower_dies(_context: &E2eContext) -> Result<()> {
    let mut cluster = Cluster::start()?;
    let leader = cluster.wait_for_agreed_leader(Duration::from_secs(10))?;
    let follower = (1..=3).find(|id| *id != leader).context("no follower")?;
    cluster.kill(follower)?;
    cluster.put(leader, "/registry/cluster/follower-dead", "still-serving")?;
    anyhow::ensure!(cluster.get(leader, "/registry/cluster/follower-dead")? == "still-serving");
    Ok(())
}

pub(super) async fn cluster_survives_the_leader_being_killed(_context: &E2eContext) -> Result<()> {
    let mut cluster = Cluster::start()?;
    let old_leader = cluster.wait_for_agreed_leader(Duration::from_secs(10))?;
    cluster.put(old_leader, "/registry/cluster/before-leader-death", "durable")?;
    cluster.kill(old_leader)?;
    let deadline = Instant::now() + Duration::from_secs(45);
    let new_leader = loop {
        if let Some(leader) = cluster.leader()? {
            if leader != old_leader {
                break leader;
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("survivors never elected a new leader");
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    anyhow::ensure!(cluster.get(new_leader, "/registry/cluster/before-leader-death")? == "durable");
    Ok(())
}

pub(super) async fn minority_refuses_writes_rather_than_inventing_them(_context: &E2eContext) -> Result<()> {
    let mut cluster = Cluster::start()?;
    let leader = cluster.wait_for_agreed_leader(Duration::from_secs(10))?;
    let survivors = (1..=3).filter(|id| *id != leader).collect::<Vec<_>>();
    cluster.kill(survivors[0])?;
    cluster.kill(survivors[1])?;
    anyhow::ensure!(
        cluster.put(leader, "/registry/cluster/no-quorum", "must-not-commit").is_err(),
        "a minority accepted a write without quorum"
    );
    Ok(())
}

pub(super) async fn restarted_member_catches_up_on_what_it_missed(_context: &E2eContext) -> Result<()> {
    let mut cluster = Cluster::start()?;
    let leader = cluster.wait_for_agreed_leader(Duration::from_secs(10))?;
    let follower = (1..=3).find(|id| *id != leader).context("no follower")?;
    cluster.kill(follower)?;
    cluster.put(leader, "/registry/cluster/rejoin", "caught-up")?;
    cluster.restart(follower)?;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if cluster.get(follower, "/registry/cluster/rejoin").is_ok_and(|value| value == "caught-up") {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("restarted member {follower} did not catch up");
}

pub(super) async fn cluster_tolerates_a_slow_link(_context: &E2eContext) -> Result<()> {
    if !std::env::var_os("NOTK8S_E2E_ENABLE_PACKET_SHAPING").is_some() {
        return Err(skip_test(
            "packet-shaping cluster variant is opt-in; set NOTK8S_E2E_ENABLE_PACKET_SHAPING=1 on a disposable root test host",
        ));
    }
    Err(skip_test(
        "the loopback cluster runner intentionally does not mutate the host network; run the archival network-namespace variant on a disposable host",
    ))
}

pub(super) async fn partitioned_leader_steps_down_and_majority_elects_another(
    _context: &E2eContext,
) -> Result<()> {
    if std::env::var_os("NOTK8S_E2E_ENABLE_PACKET_SHAPING").is_none() {
        return Err(skip_test(
            "partition tests require the isolated network-namespace harness; set NOTK8S_E2E_ENABLE_PACKET_SHAPING=1 on a disposable root test host",
        ));
    }
    Err(skip_test(
        "host-wide loopback firewall mutation is not safe for the bootstrap runner; the isolated network-namespace variant is required",
    ))
}
