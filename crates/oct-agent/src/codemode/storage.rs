//! Bounded local diagnostics. Active catalogs are never removed by retention.
use anyhow::{Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const TOTAL: u64 = 1024 * 1024 * 1024;
const PER_RUN: u64 = 128 * 1024 * 1024;
const RESERVE: u64 = 32 * 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Default)]
struct State {
    active: BTreeSet<PathBuf>,
    reserved: BTreeMap<PathBuf, u64>,
}

fn active() -> &'static Mutex<State> {
    static ACTIVE: OnceLock<Mutex<State>> = OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

pub struct Lease(PathBuf);
impl Drop for Lease {
    fn drop(&mut self) {
        // Retention starts at settlement, including after a very long run.
        if let Ok(dir) = std::fs::File::open(&self.0) {
            let _ = dir.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
        }
        let mut state = active().lock().unwrap_or_else(|p| p.into_inner());
        state.active.remove(&self.0);
        release(&mut state, &self.0);
    }
}

fn release(state: &mut State, root: &Path) {
    if let Some(value) = state.reserved.get_mut(root) {
        *value = value.saturating_sub(RESERVE);
        if *value == 0 {
            state.reserved.remove(root);
        }
    }
}

pub struct CellLease(PathBuf);
impl Drop for CellLease {
    fn drop(&mut self) {
        release(
            &mut active().lock().unwrap_or_else(|p| p.into_inner()),
            &self.0,
        );
    }
}

fn bytes(path: &Path, entries: &mut usize) -> Result<u64> {
    let meta = std::fs::symlink_metadata(path)?;
    *entries += 1;
    ensure!(
        *entries <= 100_000,
        "Code mode artifact entry limit exceeded"
    );
    if meta.is_symlink() {
        return Ok(0);
    }
    if meta.is_file() {
        return Ok(meta.len());
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        total = total.saturating_add(bytes(&entry?.path(), entries)?);
    }
    Ok(total)
}

pub async fn acquire(base: &Path, root: &Path) -> Result<Lease> {
    let base = base.to_owned();
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut active = active().lock().unwrap_or_else(|p| p.into_inner());
        std::fs::create_dir_all(&base)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut runs = Vec::new();
        let mut total = 0u64;
        let mut entries = 0;
        for conversation in std::fs::read_dir(&base)? {
            let conversation = conversation?;
            if !conversation.file_type()?.is_dir() {
                continue;
            }
            for run in std::fs::read_dir(conversation.path())? {
                let run = run?;
                if !run.file_type()?.is_dir() {
                    continue;
                }
                let path = run.path();
                let size = bytes(&path, &mut entries)?;
                total = total.saturating_add(size);
                if !active.active.contains(&path) {
                    runs.push((
                        run.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                        path,
                        size,
                    ));
                }
            }
        }
        runs.sort_by_key(|(time, _, _)| *time);
        let reserved = active.reserved.values().sum::<u64>();
        for (time, path, size) in runs {
            if SystemTime::now().duration_since(time).unwrap_or_default() > RETENTION
                || total.saturating_add(reserved).saturating_add(RESERVE) > TOTAL
            {
                std::fs::remove_dir_all(path)?;
                total = total.saturating_sub(size);
            }
        }
        ensure!(
            total.saturating_add(reserved).saturating_add(RESERVE) <= TOTAL,
            "Code mode artifact quota reached; active runs hold the retained data"
        );
        active.active.insert(root.clone());
        *active.reserved.entry(root.clone()).or_default() += RESERVE;
        Ok(Lease(root))
    })
    .await?
}

pub async fn reserve_cell(root: &Path) -> Result<CellLease> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut state = active().lock().unwrap_or_else(|p| p.into_inner());
        let reserved = state.reserved.get(&root).copied().unwrap_or_default();
        ensure!(
            bytes(&root, &mut 0)?
                .saturating_add(reserved)
                .saturating_add(RESERVE)
                <= PER_RUN,
            "Code mode run artifact quota reached; start a new run before executing more code"
        );
        let base = root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow::anyhow!("Invalid code mode artifact root"))?;
        ensure!(
            bytes(base, &mut 0)?
                .saturating_add(state.reserved.values().sum::<u64>())
                .saturating_add(RESERVE)
                <= TOTAL,
            "Code mode artifact quota reached"
        );
        *state.reserved.entry(root.clone()).or_default() += RESERVE;
        Ok(CellLease(root))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_reservations_enforce_quota_and_release_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("conv/run");
        let _run = acquire(dir.path(), &root).await.unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let first = reserve_cell(&root).await.unwrap();
        let second = reserve_cell(&root).await.unwrap();
        let third = reserve_cell(&root).await.unwrap();
        assert!(reserve_cell(&root).await.is_err());
        drop(first);
        assert!(reserve_cell(&root).await.is_ok());
        drop((second, third));
    }

    #[tokio::test]
    async fn retention_skips_active_runs_and_reclaims_inactive_runs() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("conv/old");
        std::fs::create_dir_all(&old).unwrap();
        let file = std::fs::File::create(old.join("large.log")).unwrap();
        file.set_len(TOTAL).unwrap(); // Sparse file; quota uses logical bytes.
        let current = dir.path().join("conv/current");
        let run = acquire(dir.path(), &current).await.unwrap();
        assert!(!old.exists());
        std::fs::create_dir_all(&current).unwrap();
        std::fs::File::open(&current)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
            .unwrap();
        let _next = acquire(dir.path(), &dir.path().join("conv/next"))
            .await
            .unwrap();
        assert!(current.exists());
        drop(run);
        assert!(current.metadata().unwrap().modified().unwrap() > SystemTime::UNIX_EPOCH);
    }
}
