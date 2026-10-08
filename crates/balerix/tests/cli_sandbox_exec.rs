#![allow(clippy::unwrap_used, clippy::expect_used)]
//! `balerix sandbox-exec`: the command it runs cannot create Unix sockets.
#![cfg(target_os = "linux")]

const BALERIX: &str = env!("CARGO_BIN_EXE_balerix");

/// True when python3 is usable; otherwise skips (or panics under
/// `BALERIX_REQUIRE_TOOLS=1`).
fn have_python() -> bool {
    let ok = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        assert!(
            std::env::var("BALERIX_REQUIRE_TOOLS").as_deref() != Ok("1"),
            "python3 is required (BALERIX_REQUIRE_TOOLS=1)"
        );
        eprintln!("skipping: python3 not found");
    }
    ok
}

fn under_filter(py: &str) -> std::process::Output {
    std::process::Command::new(BALERIX)
        .args(["sandbox-exec", "--", "python3", "-c", py])
        .output()
        .unwrap()
}

#[test]
fn a_unix_socket_cannot_be_created() {
    if !have_python() {
        return;
    }
    let out = under_filter(
        "import socket,errno\ntry:\n socket.socket(socket.AF_UNIX)\n print('created')\nexcept OSError as e:\n print(errno.errorcode[e.errno])",
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "EAFNOSUPPORT",
        "{out:?}"
    );
}

#[test]
fn socketpair_tcp_and_children_still_work() {
    if !have_python() {
        return;
    }
    let out = under_filter(
        "import socket,subprocess\na,b=socket.socketpair()\na.send(b'x');assert b.recv(1)==b'x'\ns=socket.socket(socket.AF_INET);s.bind(('127.0.0.1',0));s.close()\nprint(subprocess.run(['true']).returncode)",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0");
}

#[test]
fn the_filter_survives_exec_into_a_child() {
    if !have_python() {
        return;
    }
    // the child is a fresh python: it inherits the filter
    let out = under_filter(
        "import subprocess,sys\nr=subprocess.run([sys.executable,'-c','import socket\\ntry:\\n socket.socket(socket.AF_UNIX);print(1)\\nexcept OSError:\\n print(0)'],capture_output=True,text=True)\nprint(r.stdout.strip())",
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "0", "{out:?}");
}

#[cfg(target_arch = "x86_64")]
#[test]
fn the_x32_entry_is_refused_too() {
    if !have_python() {
        return;
    }
    // syscall(__X32_SYSCALL_BIT | SYS_socket, AF_UNIX, SOCK_STREAM, 0)
    let out = under_filter(
        "import ctypes,os\nlibc=ctypes.CDLL(None,use_errno=True)\nr=libc.syscall(0x40000000|41,1,1,0)\nprint(r, ctypes.get_errno())",
    );
    let text = String::from_utf8_lossy(&out.stdout);
    // EAFNOSUPPORT (97) from the filter, or ENOSYS (38) from a kernel
    // without x32; never a descriptor
    assert!(
        text.starts_with("-1 97") || text.starts_with("-1 38"),
        "{text} {out:?}"
    );
}

#[test]
fn an_empty_argv_is_an_error_not_an_unfiltered_exec() {
    let out = std::process::Command::new(BALERIX)
        .args(["sandbox-exec", "--"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn a_missing_program_exits_127() {
    let out = std::process::Command::new(BALERIX)
        .args(["sandbox-exec", "--", "/nonexistent/balerix-test-program"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(127), "{out:?}");
}

const IO_URING_PROBE: &str = "import ctypes\nlibc=ctypes.CDLL(None,use_errno=True)\nbuf=ctypes.create_string_buffer(120)\nr=libc.syscall(425,8,buf)\nprint(r, ctypes.get_errno())";

#[cfg(target_arch = "x86_64")]
#[test]
fn io_uring_is_refused_with_enosys() {
    if !have_python() {
        return;
    }
    // control: outside the filter io_uring_setup is not ENOSYS (skip when
    // the host has io_uring disabled)
    let plain = std::process::Command::new("python3")
        .args(["-c", IO_URING_PROBE])
        .output()
        .unwrap();
    if String::from_utf8_lossy(&plain.stdout)
        .trim()
        .ends_with(" 38")
    {
        eprintln!("skipping: io_uring is unavailable on this host");
        return;
    }
    let out = under_filter(IO_URING_PROBE);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "-1 38",
        "{out:?}"
    );
}
