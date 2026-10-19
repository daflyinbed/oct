//! Filesystem-only immutable asset storage. No message or database types.
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AssetStore {
    root: PathBuf,
}

impl AssetStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: std::path::absolute(root).expect("resolve attachment storage root"),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn path(&self, key: &str) -> std::io::Result<PathBuf> {
        if key.is_empty()
            || Path::new(key)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid asset storage key",
            ));
        }
        Ok(self.root.join(key))
    }
    pub async fn ensure_root(&self) -> std::io::Result<()> {
        create_private_dir(&self.root).await
    }
    pub async fn ensure_dir(&self, dir: &str) -> std::io::Result<()> {
        self.ensure_root().await?;
        create_private_dir(&self.path(dir)?).await
    }
    pub async fn create(&self, key: &str) -> std::io::Result<tokio::fs::File> {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        options.open(self.path(key)?).await
    }
    pub async fn read(&self, key: &str) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(self.path(key)?).await
    }
    pub async fn publish(&self, temp: &str, key: &str) -> std::io::Result<()> {
        tokio::fs::rename(self.path(temp)?, self.path(key)?).await
    }
    pub async fn remove(&self, key: &str) -> std::io::Result<()> {
        let path = self.path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::remove_dir(parent).await;
        }
        Ok(())
    }
}

async fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Harden existing roots too, including overrides outside the default.
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}
