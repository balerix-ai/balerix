//! A real `kube-apiserver` and `etcd` (the envtest binaries, Spec O
//! §21.3) started once per test process on free ports under `target/tmp`,
//! with the five definitions applied. nextest runs each test in a process
//! of its own, so that is one instance per test; the `envtest` test group
//! in `operator/.config/nextest.toml` bounds how many run at once. A start
//! whose port was taken between `free_port` and the bind is retried on
//! new ports. No controller-manager, no kubelet: a test patches Job and
//! Pod status itself and force-deletes pods.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{ListParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use tokio::sync::OnceCell;

pub struct EnvTest {
    pub client: Client,
    /// The operator's watch client (`watch_client`), on the same server.
    pub watches: Client,
    pub kubeconfig: PathBuf,
    /// The two watched shells; they end the servers with this process.
    _shells: Vec<Child>,
}

static INSTANCE: OnceCell<Option<EnvTest>> = OnceCell::const_new();

/// The shared instance; `None` after printing a skip (a failure under
/// `BALERIX_REQUIRE_TOOLS=1`).
pub async fn envtest() -> Option<&'static EnvTest> {
    INSTANCE.get_or_init(start).await.as_ref()
}

fn binaries() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ENVTEST_DIR").map(PathBuf::from)
        && dir.join("kube-apiserver").is_file()
        && dir.join("etcd").is_file()
    {
        return Some(dir);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find(|d| d.join("kube-apiserver").is_file() && d.join("etcd").is_file())
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `bash -c`: run the binary, and end it when this test process is gone
/// (or when the shell is sent TERM: a start retried on new ports). A test
/// binary that panics or is killed leaves no server behind. KILL, not
/// TERM, for the server: both watchers fire together, and an API server
/// whose etcd is already gone spends some 13 s in its graceful shutdown;
/// nothing here outlives the test, so there is nothing to shut down
/// gracefully.
const WATCHED: &str = r#""$0" "$@" & child=$!
trap 'kill -KILL "$child" 2>/dev/null; wait "$child"; exit 0' TERM
while kill -0 "$PPID" 2>/dev/null && kill -0 "$child" 2>/dev/null; do sleep 0.5; done
kill -KILL "$child" 2>/dev/null; wait "$child""#;

fn spawn(bin: &Path, args: &[String], log: &Path) -> Child {
    let log = std::fs::File::create(log).unwrap();
    Command::new("bash")
        .arg("-c")
        .arg(WATCHED)
        .arg(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap()
}

/// Ends a failed attempt's servers: TERM to each shell, whose trap kills
/// its server; then reaps the shell.
fn stop(shells: Vec<Child>) {
    for mut shell in shells {
        let _ = Command::new("kill").arg(shell.id().to_string()).status();
        let _ = shell.wait();
    }
}

/// Why an attempt failed. A `Collision` (a port taken by the time the
/// server bound it, or a server that died) is retried on new ports.
enum Failed {
    Collision(String),
    Fatal(String),
}

const ATTEMPTS: u32 = 4;

/// A port taken under the server, or the server gone: retry material.
fn collided(what: &str, log: &Path, shell: &mut Child) -> Option<String> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    if text.contains("address already in use") {
        return Some(format!("{what}: address already in use"));
    }
    if let Ok(Some(status)) = shell.try_wait() {
        return Some(format!("{what} exited ({status})"));
    }
    None
}

/// Whether `pid` is a live process: `/proc/<pid>/stat` exists and its
/// state is not a zombie's. Off Linux (no `/proc`), every pid is taken
/// for alive, so nothing is swept.
fn pid_alive(pid: u32) -> bool {
    if !Path::new("/proc/self").exists() {
        return true;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next())
        .is_some_and(|state| state != 'Z')
}

/// Removes every `envtest-<pid>` root under `tmp` whose test process is
/// gone. Each holds an etcd data directory with its preallocated WAL
/// (about 120 MB), and the watcher shell that ends the servers cannot
/// remove it: the servers may still be writing when it fires. Returns the
/// roots removed.
pub fn sweep_dead_roots(tmp: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("envtest-"))
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || pid_alive(pid) {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed.push(entry.path());
        }
    }
    removed
}

async fn start() -> Option<EnvTest> {
    let Some(bin) = binaries() else {
        if std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() == Ok("1") {
            panic!(
                "envtest binaries missing and BALERIX_REQUIRE_TOOLS=1 (run through `mise run operator`)"
            );
        }
        eprintln!("skip: envtest binaries (kube-apiserver, etcd) missing");
        return None;
    };
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    sweep_dead_roots(tmp);
    let root = tmp.join(format!("envtest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("certs")).unwrap();

    // the service-account signing key the API server insists on
    let key = rcgen::KeyPair::generate().unwrap();
    std::fs::write(root.join("certs/sa.key"), key.serialize_pem()).unwrap();
    std::fs::write(root.join("certs/sa.pub"), key.public_key_pem()).unwrap();

    for attempt in 1..=ATTEMPTS {
        match attempt_start(&bin, &root, attempt).await {
            Ok(env) => return Some(env),
            Err(Failed::Collision(why)) if attempt < ATTEMPTS => {
                eprintln!("envtest: attempt {attempt}: {why}; retrying on new ports");
            }
            Err(Failed::Collision(why) | Failed::Fatal(why)) => panic!(
                "{why} (attempt {attempt}); see {} and {}",
                root.join("apiserver.log").display(),
                root.join("etcd.log").display()
            ),
        }
    }
    unreachable!("the last attempt panics or returns")
}

/// One start on fresh ports: etcd serving, then the API server's
/// `/readyz` answering `ok` (its post-start hooks done), then the five
/// definitions applied and established.
async fn attempt_start(bin: &Path, root: &Path, attempt: u32) -> Result<EnvTest, Failed> {
    let etcd_log = root.join("etcd.log");
    let api_log = root.join("apiserver.log");
    let _ = std::fs::remove_dir_all(root.join("etcd"));
    std::fs::create_dir_all(root.join("etcd")).unwrap();
    // a token of this attempt's own: a client that reached another test's
    // API server on a port taken under this one is refused there
    let token = format!("envtest-{}-{attempt}", std::process::id());
    std::fs::write(
        root.join("token.csv"),
        format!("{token},admin,uid-admin,system:masters\n"),
    )
    .unwrap();

    let (etcd_client, etcd_peer, api_port) = (free_port(), free_port(), free_port());
    let mut etcd = spawn(
        &bin.join("etcd"),
        &[
            format!("--data-dir={}", root.join("etcd").display()),
            format!("--listen-client-urls=http://127.0.0.1:{etcd_client}"),
            format!("--advertise-client-urls=http://127.0.0.1:{etcd_client}"),
            format!("--listen-peer-urls=http://127.0.0.1:{etcd_peer}"),
            "--unsafe-no-fsync".to_string(),
        ],
        &etcd_log,
    );
    // etcd's own line for its client port, not a connect: a connect would
    // succeed against another test's etcd on a port taken under this one
    let serving = format!("\"address\":\"127.0.0.1:{etcd_client}\"");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(why) = collided("etcd", &etcd_log, &mut etcd) {
            stop(vec![etcd]);
            return Err(Failed::Collision(why));
        }
        let text = std::fs::read_to_string(&etcd_log).unwrap_or_default();
        if text
            .lines()
            .any(|l| l.contains("serving client traffic") && l.contains(&serving))
        {
            break;
        }
        if Instant::now() >= deadline {
            stop(vec![etcd]);
            return Err(Failed::Fatal("etcd did not come up".to_string()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut api = spawn(
        &bin.join("kube-apiserver"),
        &[
            format!("--etcd-servers=http://127.0.0.1:{etcd_client}"),
            format!("--secure-port={api_port}"),
            "--bind-address=127.0.0.1".to_string(),
            "--advertise-address=127.0.0.1".to_string(),
            format!("--cert-dir={}", root.join("certs").display()),
            "--service-cluster-ip-range=10.0.0.0/24".to_string(),
            "--authorization-mode=RBAC".to_string(),
            format!("--token-auth-file={}", root.join("token.csv").display()),
            "--service-account-issuer=https://localhost".to_string(),
            format!(
                "--service-account-key-file={}",
                root.join("certs/sa.pub").display()
            ),
            format!(
                "--service-account-signing-key-file={}",
                root.join("certs/sa.key").display()
            ),
            "--disable-admission-plugins=ServiceAccount".to_string(),
            "--allow-privileged=true".to_string(),
        ],
        &api_log,
    );

    let kubeconfig = root.join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: envtest\n  cluster:\n    server: https://127.0.0.1:{api_port}\n    insecure-skip-tls-verify: true\nusers:\n- name: admin\n  user:\n    token: {token}\ncontexts:\n- name: envtest\n  context: {{ cluster: envtest, user: admin }}\ncurrent-context: envtest\n"
        ),
    )
    .unwrap();
    let kc = Kubeconfig::read_from(&kubeconfig).unwrap();
    let config = Config::from_custom_kubeconfig(kc, &KubeConfigOptions::default())
        .await
        .unwrap();
    let watches = balerix_operator::watch_client::watch_client(config.clone()).unwrap();
    // every request bounded (§22.1): a lost one costs the test a retry, not 180 s
    let client = balerix_operator::request_client::request_client(config).unwrap();

    // `/readyz`, not `/version`: the version answers before the post-start
    // hooks (apiextensions, the system namespaces) are done
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let why = collided("kube-apiserver", &api_log, &mut api)
            .or_else(|| collided("etcd", &etcd_log, &mut etcd));
        if let Some(why) = why {
            stop(vec![etcd, api]);
            return Err(Failed::Collision(why));
        }
        let readyz = http::Request::get("/readyz").body(Vec::new()).unwrap();
        // bounded: whatever holds a port taken under this server may
        // accept and never answer, and the collision check must come round
        let answer =
            tokio::time::timeout(Duration::from_secs(2), client.request_text(readyz)).await;
        if matches!(answer, Ok(Ok(ref text)) if text.trim() == "ok") {
            break;
        }
        if Instant::now() >= deadline {
            stop(vec![etcd, api]);
            return Err(Failed::Fatal(
                "kube-apiserver did not become ready".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    for (_, yaml) in balerix_operator::api::crd_files().unwrap() {
        let crd: CustomResourceDefinition = serde_norway::from_str(&yaml).unwrap();
        crds.create(&PostParams::default(), &crd).await.unwrap();
    }

    // established: a list of each kind answers
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ok = Api::<balerix_operator::api::Daemon>::all(client.clone())
            .list(&ListParams::default())
            .await
            .is_ok()
            && Api::<balerix_operator::api::Fleet>::all(client.clone())
                .list(&ListParams::default())
                .await
                .is_ok()
            && Api::<balerix_operator::api::Crew>::all(client.clone())
                .list(&ListParams::default())
                .await
                .is_ok()
            && Api::<balerix_operator::api::Agent>::all(client.clone())
                .list(&ListParams::default())
                .await
                .is_ok()
            && Api::<balerix_operator::api::Plugin>::all(client.clone())
                .list(&ListParams::default())
                .await
                .is_ok();
        if ok {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the definitions were not established"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(EnvTest {
        client,
        watches,
        kubeconfig,
        _shells: vec![etcd, api],
    })
}
