//! Internal repository commands and snapshots, independent of HTTP DTOs.

#[derive(Debug)]
pub struct GitFile {
    pub path: String,
    pub original_path: Option<String>,
    pub index: String,
    pub worktree: String,
    pub untracked: bool,
    pub conflicted: bool,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub staged_additions: u64,
    pub staged_deletions: u64,
    pub unstaged_additions: u64,
    pub unstaged_deletions: u64,
}
#[derive(Debug)]
pub struct GitStatus {
    pub root: String,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    pub remotes: Vec<String>,
    pub files: Vec<GitFile>,
}
#[derive(Debug)]
pub struct GitEntry {
    pub id: String,
    pub short_id: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}
#[derive(Debug)]
pub struct DiffRequest {
    pub path: Option<String>,
    pub staged: Option<bool>,
    pub revision: Option<String>,
    pub stash: Option<bool>,
}
#[derive(Debug)]
pub enum GitAction {
    Stage,
    Unstage,
    Restore,
    DiscardTracked,
    TrashUntracked,
    StashAll,
    StashTracked,
    StashPop,
    Fetch,
    Pull,
    PullRebase,
    Push,
    ForcePush,
}
#[derive(Debug)]
pub struct GitActionRequest {
    pub action: GitAction,
    /// None means all eligible files; an empty selection is rejected.
    pub paths: Option<Vec<String>>,
    pub remote: Option<String>,
}
