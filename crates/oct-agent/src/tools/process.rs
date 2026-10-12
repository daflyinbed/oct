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
