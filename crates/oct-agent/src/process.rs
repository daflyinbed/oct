//! A pipe lease survives normal run cancellation only when explicitly moved
//! to the background supervisor. Backend death closes it in the kernel;
//! a tiny shell watcher then kills the entire process group, not just sh.
use std::process::Stdio;
pub(crate) fn shell(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    #[cfg(unix)]
    {
        // The watcher inherits only the read end. $$ in the subshell denotes
        // the guardian/group leader. User commands cannot hold the lease open.
        const GUARDIAN: &str = r#"
exec 3<&0
(while IFS= read -r lease <&3; do :; done; kill -KILL -$$) <&3 >/dev/null 2>&1 &
watcher=$!
sh -c "$1" </dev/null 3<&- &
worker=$!
wait "$worker"
result=$?
kill "$watcher" 2>/dev/null
wait "$watcher" 2>/dev/null
exit "$result"
"#;
        cmd.arg("-c")
            .arg(GUARDIAN)
            .arg("oct-process-guardian")
            .arg(command)
            .stdin(Stdio::piped())
            .process_group(0);
    }
    #[cfg(not(unix))]
    {
        cmd.arg("-c").arg(command).stdin(Stdio::null());
    }
    cmd.kill_on_drop(true);
    cmd
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[tokio::test]
    async fn losing_backend_pipe_lease_kills_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut cmd = super::shell(&format!(
            "trap '' TERM; echo $$ > '{}'; exec sleep 60",
            pid_file.display()
        ));
        let mut child = cmd.spawn().unwrap();
        let lease = child.stdin.take().unwrap();
        let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(s) = tokio::fs::read_to_string(&pid_file).await
                    && let Ok(pid) = s.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Simulate the kernel closing the backend's descriptors on process exit.
        // Keep Child alive so kill_on_drop cannot make this test pass by itself.
        drop(lease);
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await;
                if stat.as_ref().map_or(true, |s| {
                    s.split(") ").nth(1).is_some_and(|s| s.starts_with('Z'))
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker must die with its lease");
    }
}

/// pre_exec hook: arm PR_SET_PDEATHSIG in the forked child, before exec.
/// Runs between fork and exec, so only async-signal-safe calls are allowed —
/// `prctl` and `getppid` are raw syscall wrappers and qualify.
///
/// Complements `process_group(0)`: the process group makes *deliberate* kills
/// (timeout, cancellation, scope exit) take down the whole tree, while
/// PDEATHSIG covers the backend itself dying without a chance to run any of
/// that glue. SIGTERM (not SIGKILL) so well-behaved children can trap it,
/// clean up, and take their own descendants down.
#[cfg(all(unix, target_os = "linux"))]
pub(crate) fn set_pdeathsig() -> std::io::Result<()> {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
        // Race guard: if the parent died between fork and prctl, the signal
        // will never fire (PDEATHSIG latches onto the parent as of prctl
        // time). A reparented child (ppid == 1) means that already happened —
        // exit now instead of running orphaned.
        if libc::getppid() == 1 {
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Kills the child's process group when this scope ends. `kill_on_drop(true)`
/// only signals the `sh` process itself; backgrounded descendants share the
/// group and would otherwise survive a dropped future (agent cancelled
/// mid-execution) or a parent that exited before them. ESRCH when the group is
/// already gone is harmless.
pub(crate) struct GroupKillOnDrop(pub(crate) Option<u32>);

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0 {
            // Best effort; the group is usually already empty here.
            unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
        }
        #[cfg(not(unix))]
        let _ = self.0;
    }
}
