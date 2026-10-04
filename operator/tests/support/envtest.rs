//! A real `kube-apiserver` and `etcd` (the envtest binaries, Spec O
//! §21.3) started once per test binary on free ports under `target/tmp`,
//! with the five definitions applied. No controller-manager, no kubelet:
//! a test patches Job and Pod status itself and force-deletes pods.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::api::{ListParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};
use tokio::sync::OnceCell;

pub struct EnvTest {
    pub client: Client,
    pub kubeconfig: PathBuf,
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

/// `bash -c`: run the binary, and end it when this test process is gone.
/// A test binary that panics or is killed leaves no server behind. KILL,
/// not TERM: both watchers fire together, and an API server whose etcd is
/// already gone spends some 13 s in its graceful shutdown; nothing here
/// outlives the test, so there is nothing to shut down gracefully.
const WATCHED: &str = r#""$0" "$@" & child=$!
while kill -0 "$PPID" 2>/dev/null && kill -0 "$child" 2>/dev/null; do sleep 0.5; done
kill -KILL "$child" 2>/dev/null; wait "$child""#;

// never waited on: the shell outlives this call by design and ends with
// the test process (WATCHED), which is what reaps it
#[allow(clippy::zombie_processes)]
fn spawn(bin: &Path, args: &[String], log: &Path) {
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
        .unwrap();
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
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("envtest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("etcd")).unwrap();
    std::fs::create_dir_all(root.join("certs")).unwrap();

    // the service-account signing key the API server insists on
    let key = rcgen::KeyPair::generate().unwrap();
    std::fs::write(root.join("certs/sa.key"), key.serialize_pem()).unwrap();
    std::fs::write(root.join("certs/sa.pub"), key.public_key_pem()).unwrap();
    std::fs::write(
        root.join("token.csv"),
        "envtest-token,admin,uid-admin,system:masters\n",
    )
    .unwrap();

    let (etcd_client, etcd_peer, api_port) = (free_port(), free_port(), free_port());
    spawn(
        &bin.join("etcd"),
        &[
            format!("--data-dir={}", root.join("etcd").display()),
            format!("--listen-client-urls=http://127.0.0.1:{etcd_client}"),
            format!("--advertise-client-urls=http://127.0.0.1:{etcd_client}"),
            format!("--listen-peer-urls=http://127.0.0.1:{etcd_peer}"),
            "--unsafe-no-fsync".to_string(),
        ],
        &root.join("etcd.log"),
    );
    spawn(
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
        &root.join("apiserver.log"),
    );

    let kubeconfig = root.join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: envtest\n  cluster:\n    server: https://127.0.0.1:{api_port}\n    insecure-skip-tls-verify: true\nusers:\n- name: admin\n  user:\n    token: envtest-token\ncontexts:\n- name: envtest\n  context: {{ cluster: envtest, user: admin }}\ncurrent-context: envtest\n"
        ),
    )
    .unwrap();
    let kc = Kubeconfig::read_from(&kubeconfig).unwrap();
    let config = Config::from_custom_kubeconfig(kc, &KubeConfigOptions::default())
        .await
        .unwrap();
    let client = Client::try_from(config).unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    while client.apiserver_version().await.is_err() {
        assert!(
            Instant::now() < deadline,
            "kube-apiserver did not come up; see {}",
            root.join("apiserver.log").display()
        );
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
    Some(EnvTest { client, kubeconfig })
}
