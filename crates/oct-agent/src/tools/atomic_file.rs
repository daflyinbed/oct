//! Atomic replacement with the file lock owned by the blocking worker. A
//! dropped tool future cannot release the lock while its write is in flight.
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};
use tokio::sync::OwnedMutexGuard;
struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn write(
    path: &Path,
    content: impl AsRef<[u8]>,
    guard: OwnedMutexGuard<()>,
) -> io::Result<OwnedMutexGuard<()>> {
    let mut path = path.to_path_buf();
    let content = content.as_ref().to_vec();
    tokio::task::spawn_blocking(move || {
        // Return ownership to the caller through read-state bookkeeping.
        // If that caller was cancelled, dropping the result releases the lock.
        // Preserve writes through dangling symlinks: canonicalize cannot
        // follow a target which does not exist yet, but rename would replace
        // the link itself. Resolve its target explicitly before staging.
        for depth in 0..40 {
            if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                break;
            }
            if depth == 39 {
                return Err(io::Error::other("symlink depth exceeded"));
            }
            let target = std::fs::read_link(&path)?;
            path = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap().join(target)
            };
        }
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("missing parent directory"))?;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        #[cfg(unix)]
        if let Some(metadata) = &metadata {
            use std::os::unix::fs::MetadataExt;
            // Replacing one name of a hardlink would silently diverge aliases.
            // Refuse rather than changing the write contract or truncating data.
            if metadata.nlink() > 1 {
                return Err(io::Error::other(
                    "atomic write refuses a multiply-linked file",
                ));
            }
        }
        let temporary = Temporary(parent.join(format!(".oct-write-{}", uuid::Uuid::new_v4())));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o666);
        }
        let mut file = options.open(&temporary.0)?;
        if let Some(metadata) = metadata {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(&content)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary.0, &path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(guard)
    })
    .await
    .map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn successful_replacement_keeps_lock_until_caller_records_read_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "old").unwrap();
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let guard = super::write(&path, "new", lock.clone().lock_owned().await)
            .await
            .unwrap();
        assert!(lock.try_lock().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new");
        drop(guard);
        assert!(lock.try_lock().is_ok());
    }
}
