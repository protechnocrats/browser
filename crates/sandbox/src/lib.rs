//! The renderer process's sandbox (ADR 016 §6).
//!
//! [`enter`] is called once, by the renderer binary, after its start-up work
//! and before it reads anything from a page. On Linux it
//!
//! - closes the filesystem with **Landlock**: every access right the running
//!   kernel knows is handled, and none is granted, so no file can be opened
//!   (files already open — the pipes to the browser — keep working);
//! - installs a **seccomp** filter (on every thread) under which `socket`,
//!   `socketpair`, `connect`, `bind`, `listen`, `accept`, `accept4`,
//!   `execve`, `execveat`, `ptrace`, `process_vm_readv` and
//!   `process_vm_writev` fail with `EPERM`: no network of its own, no new
//!   programs, no reaching into other processes.
//!
//! Both also set `no_new_privs`. Elsewhere, and on a kernel without
//! Landlock, the report says what is missing; the process boundary still
//! holds.

#![forbid(unsafe_code)]

/// What [`enter`] managed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The filesystem is closed and the system-call filter installed.
    pub active: bool,
    /// One line for diagnostics.
    pub detail: String,
}

/// System calls the renderer never needs, and a compromised one would.
#[cfg(target_os = "linux")]
const DENIED_SYSCALLS: &[i64] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_execve,
    libc::SYS_execveat,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
];

/// Sandboxes the calling process. Never fails outright: what could not be
/// applied is in the report.
pub fn enter() -> Report {
    #[cfg(target_os = "linux")]
    {
        let filesystem = linux::close_filesystem();
        let syscalls = linux::filter_syscalls();
        let active = filesystem.is_ok() && syscalls.is_ok();
        let part = |r: &Result<&'static str, String>| match r {
            Ok(s) => s.to_string(),
            Err(e) => format!("failed: {e}"),
        };
        Report { active, detail: format!("filesystem: {}; system calls: {}", part(&filesystem), part(&syscalls)) }
    }
    #[cfg(not(target_os = "linux"))]
    {
        Report { active: false, detail: format!("no system sandbox on {} yet: process isolation only", std::env::consts::OS) }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use landlock::{Access, AccessFs, Ruleset, RulesetAttr, RulesetStatus, ABI};
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
    use std::collections::BTreeMap;

    pub fn close_filesystem() -> Result<&'static str, String> {
        let abi = ABI::V5;
        let status = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| e.to_string())?
            .create()
            .map_err(|e| e.to_string())?
            .restrict_self()
            .map_err(|e| e.to_string())?;
        match status.ruleset {
            RulesetStatus::FullyEnforced => Ok("closed (Landlock)"),
            RulesetStatus::PartiallyEnforced => Ok("closed to what this kernel's Landlock covers"),
            RulesetStatus::NotEnforced => Err("this kernel does not enforce Landlock".into()),
        }
    }

    pub fn filter_syscalls() -> Result<&'static str, String> {
        let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = super::DENIED_SYSCALLS.iter().map(|s| (*s, Vec::new())).collect();
        let arch = std::env::consts::ARCH.try_into().map_err(|e: seccompiler::BackendError| e.to_string())?;
        let filter = SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32), arch).map_err(|e| e.to_string())?;
        let program: BpfProgram = filter.try_into().map_err(|e: seccompiler::BackendError| e.to_string())?;
        seccompiler::apply_filter_all_threads(&program).map_err(|e| e.to_string())?;
        Ok("sockets, exec and ptrace denied (seccomp)")
    }
}
