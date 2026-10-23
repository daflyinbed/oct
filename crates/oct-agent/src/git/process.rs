//! Bounded, non-interactive Git subprocess execution.
use crate::error::{AppError, AppResult};
use std::time::Duration;
use tokio::{io::AsyncReadExt, process::Command};

pub(super) const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;

async fn read_bounded(reader: impl tokio::io::AsyncRead + Unpin) -> AppResult<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(anyhow::Error::from)?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(AppError::BadRequest(
            "Git output exceeds 4 MiB; select a smaller diff".into(),
        ));
    }
    Ok(bytes)
}

pub(super) async fn output(
    root: &std::path::Path,
    args: &[&str],
    allow_diff_exit: bool,
) -> AppResult<String> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args([
            "--no-pager",
            "-c",
            "color.ui=false",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // stash invokes Git internally with generated pathspecs; literal mode there
    // prevents its cleanup from removing the files it just saved.
    if args.first() != Some(&"stash") {
        command.env("GIT_LITERAL_PATHSPECS", "1");
    }
    let mut child = command.spawn().map_err(anyhow::Error::from)?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let (out, err) = tokio::try_join!(read_bounded(stdout), read_bounded(stderr))?;
        let status = child.wait().await.map_err(anyhow::Error::from)?;
        let diff_only_failure = allow_diff_exit && status.code() == Some(1);
        if !status.success() && !diff_only_failure {
            return Err(AppError::Conflict(
                String::from_utf8_lossy(&err).trim().to_owned(),
            ));
        }
        String::from_utf8(out)
            .map_err(|_| AppError::BadRequest("Git output contains non-UTF-8 paths or text".into()))
    })
    .await;
    result.map_err(|_| {
        AppError::Conflict(
            "Git operation timed out; refresh repository state before retrying".into(),
        )
    })?
}
