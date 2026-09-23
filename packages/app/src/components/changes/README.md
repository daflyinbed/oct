# Changes panel

The panel reads the selected conversation's project repository, including linked Git worktrees. Set the project's primary working directory to the repository root; nested directories and additional working directories are not treated as permission to change a parent repository.

## Interface

- Changes shows tracked/untracked or staged/unstaged groups, or an ungrouped list. Tree/list and grouping preferences persist locally.
- Checkboxes stage or unstage complete files, directories or groups. A partially staged file appears in both staged/unstaged groups, with separate line counts and diff views. The ungrouped view shows its net change against HEAD.
- View Diff opens the complete working tree diff; clicking a file opens its diff. Binary files and nested repositories display a textual indication. History shows the latest 100 commits and opens commit diffs. View Stash lists saved stashes and opens their patches.
- Status refreshes after actions, on window focus and every five seconds while visible. Errors remain visible until an explicit refresh/focus or another action. Failed operations also refresh status because a pull or stash pop may leave conflicts.
- No new keyboard shortcuts are registered.

## Operations

| Action | Behavior |
| --- | --- |
| Stage / Unstage | Update the index; unstage also works before the first commit |
| Restore All Changes | Restore tracked worktree and index to HEAD; requires confirmation |
| Discard Tracked Changes | Restore tracked worktree from the index; keep staged changes; requires confirmation |
| Trash Untracked Files | Move untracked files to the desktop Trash using `gio trash`; ignored files are untouched; requires confirmation |
| Stash All / Tracked | Save with / without untracked files |
| Stash Pop | Apply and remove the latest stash on success; Git keeps conflicted stashes |
| Fetch / Fetch From | Fetch default or selected configured remote |
| Pull | Fast-forward only |
| Pull (Rebase) | Pull with rebase |
| Push / Push To | Push to default or selected configured remote; Git's upstream/refspec configuration applies |
| Force Push | `--force-with-lease`, after confirmation |

Git must be installed. Trash requires `gio`; missing or unsupported Trash fails explicitly rather than deleting permanently. Authentication uses the host's configured Git credentials/SSH agent; interactive terminal prompts are disabled. No real model or external remote is used in tests.

## API

All paths start with `/api/projects/{projectId}/git`:

| Method | Path | Response / request |
| --- | --- | --- |
| GET | `/status` | Branch, upstream, ahead/behind, remotes and per-file status/statistics |
| GET | `/diff` | `{ patch }`; optional `path`, `staged`, `revision`, `stash` query parameters |
| GET | `/history` | Latest 100 commits |
| GET | `/stashes` | Saved stash entries |
| POST | `/actions` | `{ action, paths?, remote? }`; `paths` omitted means all eligible changes |

File selections must match current Git status; operands are literal paths, never shell expressions. Revision previews accept full object IDs. Operations within one repository are serialized. Commands have a 120-second deadline and 4 MiB output limit; oversized diffs report an error so a smaller file diff can be selected. Network failures, conflicts and missing upstream configuration are returned as actionable errors. A timeout may have partially changed repository state, so clients refresh before retrying.

OpenAPI and generated frontend types describe the exact request/response shapes. Regression coverage is in `crates/oct-agent/tests/git_api.rs`, `useGit.test.ts` and `browser-e2e/changes.test.ts`.
