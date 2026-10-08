//! The filter behind `balerix sandbox-exec` (Unix socket policy `deny`).
//! Two stacked seccomp filters, because a filter has one match action:
//!
//! - `socket(AF_UNIX, ..)` fails with `EAFNOSUPPORT`, so a sandboxed
//!   process cannot reach a Unix socket outside its grants (nono leaves
//!   pathname sockets unmediated by default). `socketpair` stays: pipes
//!   between a process and its children are not a way out.
//! - `io_uring_setup`, `io_uring_enter` and `io_uring_register` fail with
//!   `ENOSYS`: on Linux 5.19 and later io_uring can create and connect a
//!   socket without calling `socket`. `ENOSYS` makes callers such as libuv
//!   fall back to plain syscalls.
//!
//! The filters match disjoint syscalls, so stacking them (the kernel takes
//! the most restrictive result) keeps each errno.
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;
use std::io;

/// x32 syscalls pass the x86_64 architecture check, so `socket` needs its
/// own entry there.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: i64 = 0x4000_0000;

fn build(
    rules: BTreeMap<i64, Vec<SeccompRule>>,
    errno: i32,
) -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let arch: TargetArch = std::env::consts::ARCH.try_into()?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,               // no rule matched
        SeccompAction::Errno(errno as u32), // a rule matched
        arch,
    )?;
    Ok(filter.try_into()?)
}

/// Syscall numbers with their x32 twin on x86_64.
fn with_x32(nr: i64) -> Vec<i64> {
    #[cfg(target_arch = "x86_64")]
    return vec![nr, X32_SYSCALL_BIT | nr];
    #[cfg(not(target_arch = "x86_64"))]
    vec![nr]
}

fn unix_socket_program() -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let unix = || -> Result<Vec<SeccompRule>, seccompiler::BackendError> {
        Ok(vec![SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )?])?])
    };
    let mut rules = BTreeMap::new();
    for nr in with_x32(libc::SYS_socket) {
        rules.insert(nr, unix()?);
    }
    build(rules, libc::EAFNOSUPPORT)
}

fn io_uring_program() -> Result<BpfProgram, Box<dyn std::error::Error>> {
    let mut rules = BTreeMap::new();
    for sys in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        for nr in with_x32(sys) {
            rules.insert(nr, Vec::new()); // an empty chain matches always
        }
    }
    build(rules, libc::ENOSYS)
}

fn programs() -> Result<[BpfProgram; 2], Box<dyn std::error::Error>> {
    Ok([unix_socket_program()?, io_uring_program()?])
}

/// Installs the filter on the calling thread (and so on everything it
/// execs): `socket(AF_UNIX, …)` fails with `EAFNOSUPPORT`, and the
/// io_uring syscalls (`io_uring_setup`, `_enter`, `_register`), which can
/// create sockets without `socket()`, fail with `ENOSYS`; both include
/// the x32 entries on x86_64; a foreign-arch syscall is fatal.
/// `apply_filter` sets `PR_SET_NO_NEW_PRIVS` first.
pub fn deny_unix_sockets() -> io::Result<()> {
    let progs =
        programs().map_err(|e| io::Error::other(format!("cannot build the filter: {e}")))?;
    for prog in &progs {
        seccompiler::apply_filter(prog)
            .map_err(|e| io::Error::other(format!("cannot install the filter: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn the_program_builds_for_this_arch() {
        for p in super::programs().unwrap() {
            assert!(!p.is_empty());
        }
    }
}
