//! `(mova.os/pid-alive? pid)` -> boolean. Uses libc::kill(pid, 0) to check if process exists. `(mova.os/pid)` -> own pid.
use std::sync::Arc;

use crate::error::RjError;
use crate::eval::Interp;
use crate::value::{NativeFn, Symbol, Value};

#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    use std::io;
    unsafe {
        // kill(pid, 0) returns 0 if process exists, -1 on error.
        // errno == ESRCH means process not found; errno == EPERM means process exists but not accessible.
        if libc::kill(pid, 0) == 0 {
            return true;
        }
        // kill failed; check if EPERM (process exists but no permission) or ESRCH (doesn't exist)
        let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        err == libc::EPERM
    }
}

#[cfg(not(unix))]
fn pid_alive(_pid: i32) -> bool {
    // On non-Unix systems, we can't reliably check, so return true.
    true
}

/// `[phys_footprint, lifetime max phys_footprint]` of this process in bytes: the number `footprint(1)` and Activity
/// Monitor show (same ledger as `task_info` TASK_VM_INFO, read with one `proc_pid_rusage` call).
#[cfg(target_os = "macos")]
pub(crate) fn footprint() -> Option<(u64, u64)> {
    unsafe {
        let mut ri: libc::rusage_info_v4 = std::mem::zeroed();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, &mut ri as *mut _ as *mut libc::rusage_info_t) != 0 {
            return None;
        }
        Some((ri.ri_phys_footprint, ri.ri_lifetime_max_phys_footprint))
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn footprint() -> Option<(u64, u64)> {
    None
}

pub(crate) fn register(i: &mut Interp) {
    // (mova.os/footprint) -> [bytes peak-bytes] (phys_footprint now and its lifetime max), nil where it is not known.
    let f = NativeFn::new("footprint", |_i: &mut Interp, _a: &[Value]| {
        Ok(match footprint() {
            Some((b, peak)) => Value::Vector([Value::Int(b as i64), Value::Int(peak as i64)].into_iter().collect()),
            None => Value::Nil,
        })
    });
    i.globals.set_builtin(Symbol { ns: Some("mova.os".into()), name: "footprint".into() }, Value::Native(Arc::new(f)));
    let f = NativeFn::new("pid-alive?", |_i: &mut Interp, a: &[Value]| {
        let Some(Value::Int(pid_val)) = a.first() else {
            return Err(RjError::type_err(format!(
                "mova.os/pid-alive?: expected an int, got {}",
                a.first().map(|v| v.type_name()).unwrap_or("nothing")
            )));
        };
        // Reject pid <= 0 (process group targets) and > i32::MAX (invalid)
        if *pid_val <= 0 || *pid_val > i32::MAX as i64 {
            return Err(RjError::other(format!(
                "mova.os/pid-alive?: pid must be 1..{}, got {}",
                i32::MAX,
                pid_val
            )));
        }
        Ok(Value::Bool(pid_alive(*pid_val as i32)))
    });
    i.globals.set_builtin(
        Symbol { ns: Some("mova.os".into()), name: "pid-alive?".into() },
        Value::Native(Arc::new(f))
    );
    // (mova.os/pid) -> this process id.
    let f = NativeFn::new("pid", |_i: &mut Interp, _a: &[Value]| Ok(Value::Int(std::process::id() as i64)));
    i.globals.set_builtin(Symbol { ns: Some("mova.os".into()), name: "pid".into() }, Value::Native(Arc::new(f)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_alive_own_process() {
        let own_pid = std::process::id() as i32;
        assert!(pid_alive(own_pid), "own process should be alive");
    }

    #[test]
    fn pid_alive_spawned_child() {
        use std::process::Command;
        let child = Command::new("sh").arg("-c").arg("sleep 10").spawn();
        if let Ok(mut child) = child {
            let child_pid = child.id() as i32;
            assert!(pid_alive(child_pid), "spawned child should be alive");
            child.kill().ok();
            let _ = child.wait();
            assert!(!pid_alive(child_pid), "child should be dead after wait");
        }
    }

    #[test]
    fn pid_alive_init_process() {
        // PID 1 (init) should exist and we don't have permission to send it signals
        // This exercises the EPERM path
        assert!(pid_alive(1), "init process (pid 1) should return true (EPERM path)");
    }
}
