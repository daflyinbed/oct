//! Embedded frontend bundle: the Vite build output of `packages/app` is baked
//! into the binary (release) or read from `packages/app/dist` at runtime
//! (debug), and served through `ServeDir` with a history-mode SPA fallback.
//!
//! `allow_missing` keeps Rust-only builds compiling before the frontend has
//! ever been built — the asset table is then empty, `/` answers with a build
//! hint and `main` logs a startup warning. Release binaries therefore need
//! `pnpm --filter frontend build` to run BEFORE `cargo build --release`
//! (stale `dist/` embeds stale assets silently).

use axum::{
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;
use std::{
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncSeek, ReadBuf};
use tower_http::services::fs::{Backend, File as FsFile, Metadata as FsMetadata};

const INDEX_HTML: &str = "index.html";

#[derive(Embed)]
#[folder = "../../packages/app/dist"]
#[allow_missing = true]
struct Assets;

#[derive(Clone, Copy)]
pub struct EmbedMetadata {
    len: u64,
    last_modified: Option<u64>,
}

impl FsMetadata for EmbedMetadata {
    fn is_dir(&self) -> bool {
        false
    }

    fn modified(&self) -> io::Result<SystemTime> {
        self.last_modified
            .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
            .ok_or_else(|| io::Error::other("embedded file has no last-modified timestamp"))
    }

    fn len(&self) -> u64 {
        self.len
    }
}

pub struct EmbedFile {
    cursor: std::io::Cursor<std::borrow::Cow<'static, [u8]>>,
    metadata: EmbedMetadata,
}

impl AsyncRead for EmbedFile {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.cursor).poll_read(cx, buf)
    }
}

impl AsyncSeek for EmbedFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        Pin::new(&mut self.cursor).start_seek(position)
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.cursor).poll_complete(cx)
    }
}

impl FsFile for EmbedFile {
    type Metadata = EmbedMetadata;
    type MetadataFuture<'a> = std::future::Ready<io::Result<EmbedMetadata>>;

    fn metadata(&self) -> Self::MetadataFuture<'_> {
        std::future::ready(Ok(self.metadata))
    }
}

#[derive(Clone, Copy)]
pub struct EmbedBackend;

impl Backend for EmbedBackend {
    type File = EmbedFile;
    type Metadata = EmbedMetadata;
    type OpenFuture = std::future::Ready<io::Result<EmbedFile>>;
    type MetadataFuture = std::future::Ready<io::Result<EmbedMetadata>>;

    fn open(&self, path: PathBuf) -> Self::OpenFuture {
        std::future::ready(lookup(&path).map(|file| {
            let metadata = embed_metadata(&file);
            EmbedFile {
                cursor: std::io::Cursor::new(file.data),
                metadata,
            }
        }))
    }

    fn metadata(&self, path: PathBuf) -> Self::MetadataFuture {
        std::future::ready(lookup(&path).map(|file| embed_metadata(&file)))
    }
}

fn lookup(path: &Path) -> io::Result<rust_embed::EmbeddedFile> {
    // `ServeDir::with_backend` prefixes requested paths with its `.` base,
    // while rust-embed stores paths relative to the embedded folder.
    let path = path.strip_prefix(".").unwrap_or(path);
    let key = path.to_string_lossy().replace('\\', "/");
    Assets::get(&key).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}

fn embed_metadata(file: &rust_embed::EmbeddedFile) -> EmbedMetadata {
    EmbedMetadata {
        len: file.data.len() as u64,
        last_modified: file.metadata.last_modified(),
    }
}

/// Whether the frontend bundle is available (embedded, or present on disk in
/// debug builds). False right after a fresh checkout or a Rust-only build.
pub fn frontend_available() -> bool {
    Assets::get(INDEX_HTML).is_some()
}

pub async fn spa_index() -> Response {
    match Assets::get(INDEX_HTML) {
        Some(content) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content.data,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "frontend bundle not found: run `pnpm --filter frontend build` first \
             (release binaries embed packages/app/dist at compile time)",
        )
            .into_response(),
    }
}

/// SPA fallback behind `ServeDir`: history-mode deep links (and any unknown
/// path) render `index.html`, while unmatched `/api` and `/scalar` paths keep
/// a real 404 instead of feeding HTML to JSON clients.
pub async fn spa_fallback(uri: Uri) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") || path.starts_with("/scalar") {
        return (StatusCode::NOT_FOUND, "404").into_response();
    }
    spa_index().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_accepts_serve_dir_relative_paths() {
        // Fresh checkouts may not have built the frontend yet; the prefix
        // strip is only observable when the asset table is non-empty.
        if !frontend_available() {
            return;
        }
        assert!(lookup(Path::new("./index.html")).is_ok());
    }

    #[test]
    fn lookup_rejects_missing_files() {
        assert!(lookup(Path::new("definitely/not/here.js")).is_err());
    }
}
