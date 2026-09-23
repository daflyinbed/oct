//! Real Git repositories + HTTP, with no model or remote network access.
mod support;
use serde_json::{Value, json};
use std::path::Path;
use support::TestApp;

fn git(root: &Path, args: &[&str]) -> String {
    let result = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}
async fn setup(commit: bool) -> (TestApp, tempfile::TempDir, String) {
    let app = TestApp::new().await;
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    std::fs::write(dir.path().join("tracked.txt"), "one\ntwo\n").unwrap();
    if commit {
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "initial"]);
    }
    let response = app
        .post_json(
            "/api/projects",
            json!({"name":"git", "working_dir":dir.path()}),
        )
        .await;
    assert_eq!(response.status(), 201);
    let p: Value = response.json().await.unwrap();
    (
        app,
        dir,
        format!("/api/projects/{}/git", p["id"].as_str().unwrap()),
    )
}
async fn get(app: &TestApp, url: &str) -> Value {
    let result = app.get_json(url).await;
    assert_eq!(result.status(), 200, "{}", result.text().await.unwrap());
    result.json().await.unwrap()
}
async fn action(app: &TestApp, base: &str, body: Value) {
    let result = app.post_json(&format!("{base}/actions"), body).await;
    assert_eq!(result.status(), 200, "{}", result.text().await.unwrap());
}
#[tokio::test]
async fn status_partial_stage_rename_unstage_restore_and_literal_paths() {
    let (app, dir, base) = setup(true).await;
    std::fs::write(dir.path().join("tracked.txt"), "one\nchanged\n").unwrap();
    let strange = ":(glob)* 中文\nfile.txt";
    std::fs::write(dir.path().join(strange), "new\n").unwrap();
    std::fs::write(dir.path().join("binary.bin"), [0, 1, 2]).unwrap();
    let state = get(&app, &format!("{base}/status")).await;
    assert_eq!(state["files"].as_array().unwrap().len(), 3);
    let tracked = state["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(tracked["additions"], 1);
    assert_eq!(tracked["deletions"], 1);
    assert!(
        state["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "binary.bin" && f["binary"] == true)
    );
    action(&app, &base, json!({"action":"stage","paths":[strange]})).await;
    assert_eq!(
        git(dir.path(), &["diff", "--cached", "--name-only", "-z"]),
        format!("{strange}\0")
    );
    action(
        &app,
        &base,
        json!({"action":"stage","paths":["tracked.txt"]}),
    )
    .await;
    std::fs::write(dir.path().join("tracked.txt"), "one\nchanged\nthree\n").unwrap();
    let patch = get(&app, &format!("{base}/diff?path=tracked.txt&staged=true")).await;
    assert!(patch["patch"].as_str().unwrap().contains("+changed"));
    assert!(!patch["patch"].as_str().unwrap().contains("+three"));
    let patch = get(&app, &format!("{base}/diff?path=tracked.txt&staged=false")).await;
    assert!(patch["patch"].as_str().unwrap().contains("+three"));
    let partial = get(&app, &format!("{base}/status")).await;
    let file = partial["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "tracked.txt")
        .unwrap();
    assert_eq!(file["staged_additions"], 1);
    assert_eq!(file["staged_deletions"], 1);
    assert_eq!(file["unstaged_additions"], 1);
    assert_eq!(file["unstaged_deletions"], 0);
    action(&app, &base, json!({"action":"unstage"})).await;
    assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "");
    action(&app, &base, json!({"action":"restore"})).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(dir.path().join(strange).exists());
    git(dir.path(), &["mv", "tracked.txt", "renamed.txt"]);
    let state = get(&app, &format!("{base}/status")).await;
    assert!(
        state["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["original_path"] == "tracked.txt")
    );
    action(
        &app,
        &base,
        json!({"action":"stage","paths":["renamed.txt"]}),
    )
    .await;
    action(
        &app,
        &base,
        json!({"action":"unstage","paths":["renamed.txt"]}),
    )
    .await;
    assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "");
    assert_eq!(
        app.post_json(
            &format!("{base}/actions"),
            json!({"action":"stage","paths":["../escape"]})
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        app.get_json(&format!("{base}/diff?path=../escape"))
            .await
            .status(),
        404
    );
    assert_eq!(
        app.get_json(&format!("{base}/diff?revision=--output=/tmp/x"))
            .await
            .status(),
        400
    );
}
#[tokio::test]
async fn unborn_history_stash_roundtrip_and_worktree_support() {
    let (app, dir, base) = setup(false).await;
    assert_eq!(get(&app, &format!("{base}/history")).await, json!([]));
    action(&app, &base, json!({"action":"stage"})).await;
    assert!(
        get(&app, &format!("{base}/diff")).await["patch"]
            .as_str()
            .unwrap()
            .contains("+one")
    );
    std::fs::write(dir.path().join("tracked.txt"), "latest\n").unwrap();
    let net = get(&app, &format!("{base}/diff")).await;
    let net = net["patch"].as_str().unwrap();
    assert_eq!(net.matches("diff --git").count(), 1);
    assert!(net.contains("+latest"));
    assert!(!net.contains("+one"));
    std::fs::write(dir.path().join("tracked.txt"), "one\ntwo\n").unwrap();
    action(&app, &base, json!({"action":"unstage"})).await;
    assert!(dir.path().join("tracked.txt").exists());
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "initial 中文"]);
    let history = get(&app, &format!("{base}/history")).await;
    assert_eq!(history[0]["subject"], "initial 中文");
    let sha = history[0]["id"].as_str().unwrap();
    assert!(
        get(&app, &format!("{base}/diff?revision={sha}")).await["patch"]
            .as_str()
            .unwrap()
            .contains("+one")
    );
    std::fs::write(dir.path().join("tracked.txt"), "edited\n").unwrap();
    std::fs::write(dir.path().join("new.txt"), "untracked\n").unwrap();
    action(&app, &base, json!({"action":"stash_tracked"})).await;
    assert!(dir.path().join("new.txt").exists());
    let stashes = get(&app, &format!("{base}/stashes")).await;
    assert_eq!(stashes.as_array().unwrap().len(), 1);
    let sha = stashes[0]["id"].as_str().unwrap();
    assert!(
        get(&app, &format!("{base}/diff?revision={sha}&stash=true")).await["patch"]
            .as_str()
            .unwrap()
            .contains("+edited")
    );
    action(&app, &base, json!({"action":"stash_pop"})).await;
    action(&app, &base, json!({"action":"stash_all"})).await;
    assert!(!dir.path().join("new.txt").exists());
    action(&app, &base, json!({"action":"stash_pop"})).await;
    assert!(dir.path().join("new.txt").exists());
    let worktree = dir.path().join("linked");
    git(
        dir.path(),
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            worktree.to_str().unwrap(),
        ],
    );
    let p: Value = app
        .post_json(
            "/api/projects",
            json!({"name":"linked", "working_dir":worktree}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        get(
            &app,
            &format!("/api/projects/{}/git/status", p["id"].as_str().unwrap())
        )
        .await["branch"],
        "linked"
    );
}
#[tokio::test]
async fn local_remote_fetch_push_pull_and_errors() {
    let (app, dir, base) = setup(true).await;
    assert_eq!(
        app.post_json(&format!("{base}/actions"), json!({"action":"fetch"}))
            .await
            .status(),
        400
    );
    let remote = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "--bare", "-b", "main"]);
    git(
        dir.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    git(dir.path(), &["push", "-u", "origin", "main"]);
    std::fs::write(dir.path().join("tracked.txt"), "local\n").unwrap();
    git(dir.path(), &["commit", "-am", "next"]);
    assert_eq!(get(&app, &format!("{base}/status")).await["ahead"], 1);
    for name in ["push", "fetch", "pull", "pull_rebase", "force_push"] {
        action(&app, &base, json!({"action":name,"remote":"origin"})).await;
    }
    assert_eq!(get(&app, &format!("{base}/status")).await["ahead"], 0);
    assert_eq!(
        app.post_json(
            &format!("{base}/actions"),
            json!({"action":"fetch", "remote":"--upload-pack=bad"})
        )
        .await
        .status(),
        400
    );
    let plain = tempfile::tempdir().unwrap();
    let p: Value = app
        .post_json(
            "/api/projects",
            json!({"name":"plain", "working_dir":plain.path()}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        app.get_json(&format!(
            "/api/projects/{}/git/status",
            p["id"].as_str().unwrap()
        ))
        .await
        .status(),
        400
    );
    assert_eq!(
        app.get_json("/api/projects/missing/git/status")
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn conflicts_and_bounded_diff_are_reported_without_hanging() {
    let (app, dir, base) = setup(true).await;
    git(dir.path(), &["checkout", "-b", "other"]);
    std::fs::write(dir.path().join("tracked.txt"), "other\n").unwrap();
    git(dir.path(), &["commit", "-am", "other"]);
    git(dir.path(), &["checkout", "main"]);
    std::fs::write(dir.path().join("tracked.txt"), "main\n").unwrap();
    git(dir.path(), &["commit", "-am", "main"]);
    let merge = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["merge", "other"])
        .output()
        .unwrap();
    assert!(!merge.status.success());
    let state = get(&app, &format!("{base}/status")).await;
    assert_eq!(state["files"][0]["conflicted"], true);
    action(&app, &base, json!({"action":"restore"})).await;
    assert_eq!(
        get(&app, &format!("{base}/status")).await["files"],
        json!([])
    );
    std::fs::write(dir.path().join("large.txt"), "x".repeat(5 * 1024 * 1024)).unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.get_json(&format!("{base}/diff?path=large.txt")),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
}
