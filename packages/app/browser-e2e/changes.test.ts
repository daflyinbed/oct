import { expect, it } from "vitest";
import { page } from "vitest/browser";
import { mountApp } from "./support/app";
import { conversationFixture, projectFixture } from "./support/fixtures";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { components } from "@/api/schema";

function file(
  path: string,
  untracked = false,
): components["schemas"]["GitFile"] {
  return {
    path,
    original_path: null,
    index: untracked ? "?" : " ",
    worktree: untracked ? "?" : "M",
    untracked,
    conflicted: false,
    additions: 10,
    deletions: 2,
    binary: false,
    staged_additions: 0,
    staged_deletions: 0,
    unstaged_additions: 10,
    unstaged_deletions: 2,
  };
}

it("changes: real panel groups, diff, staging, history, stash and destructive confirmation", async () => {
  await resetMock();
  localStorage.removeItem("oct-changes-view");
  localStorage.removeItem("oct-changes-grouping");
  await setScenario({
    projects: [projectFixture()],
    conversations: [conversationFixture()],
    providers: [],
    git: {
      status: {
        root: "/tmp/demo",
        branch: "main",
        upstream: "origin/main",
        ahead: 4,
        behind: 0,
        remotes: ["origin"],
        files: [file("src/pages/conversation.vue"), file(".zcodeignore", true)],
      },
      history: [
        {
          id: "a".repeat(40),
          short_id: "aaaaaaa",
          author: "Test",
          date: "2026-09-23",
          subject: "Implement Changes",
        },
      ],
      stashes: [
        {
          id: "b".repeat(40),
          short_id: "stash@{0}",
          author: "Test",
          date: "2026-09-23",
          subject: "Saved changes",
        },
      ],
      patch:
        "diff --git a/src/pages/conversation.vue b/src/pages/conversation.vue\n@@ -1 +1 @@\n-old\n+new",
    },
  });
  const mounted = await mountApp();
  try {
    await mounted.router.push("/conversation/c1");
    await expect
      .poll(() => document.body.textContent)
      .toContain(".zcodeignore");
    await page.getByText("conversation.vue", { exact: true }).click();
    await expect.poll(() => document.body.textContent).toContain("@@ -1 +1 @@");
    await page
      .getByRole("checkbox", {
        name: "Stage src/pages/conversation.vue",
        exact: true,
      })
      .click();
    await expect
      .poll(async () =>
        (await getRequests())
          .filter((r) => r.path.endsWith("/git/actions"))
          .map((r) => r.body),
      )
      .toContainEqual({
        action: "stage",
        paths: ["src/pages/conversation.vue"],
      });
    await expect
      .poll(
        () =>
          (
            page
              .getByRole("checkbox", {
                name: "Stage src/pages/conversation.vue",
                exact: true,
              })
              .element() as HTMLInputElement
          ).checked,
      )
      .toBe(true);
    await page
      .getByRole("button", { name: "View options", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "List", exact: true }).click();
    await expect
      .poll(() => document.body.textContent)
      .toContain("src/pages/conversation.vue");
    await page
      .getByRole("button", { name: "View options", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Staged & Unstaged" }).click();
    await expect
      .poll(() => document.querySelector("#diff-panel")?.textContent)
      .toContain("Unstaged");
    await page
      .getByRole("checkbox", { name: "Stage Staged", exact: true })
      .click();
    await expect
      .poll(async () =>
        (await getRequests())
          .filter((r) => r.path.endsWith("/git/actions"))
          .map((r) => r.body),
      )
      .toContainEqual({
        action: "unstage",
        paths: ["src/pages/conversation.vue"],
      });
    await page.getByRole("button", { name: "History", exact: true }).click();
    await expect
      .poll(() => document.body.textContent)
      .toContain("Implement Changes");
    await page.getByText("Implement Changes", { exact: true }).click();
    await expect
      .poll(() => document.body.textContent)
      .toContain("Commit aaaaaaaa");
    await page
      .getByRole("button", { name: "Changes actions", exact: true })
      .click();
    await page
      .getByRole("menuitem", { name: "View Stash", exact: true })
      .click();
    await expect
      .poll(() => document.body.textContent)
      .toContain("Saved changes");
    await page
      .getByRole("button", { name: "Changes actions", exact: true })
      .click();
    await page
      .getByRole("menuitem", { name: "Restore All Changes", exact: true })
      .click();
    await expect
      .poll(() => page.getByRole("alertdialog").elements().length)
      .toBe(1);
    await page.getByRole("button", { name: "取消", exact: true }).click();
    expect(
      (await getRequests()).filter((r) => r.path.endsWith("/git/actions")),
    ).toHaveLength(2);
    await page
      .getByRole("button", { name: "Remote actions", exact: true })
      .click();
    await page
      .getByRole("menuitem", { name: "Force Push (with lease)", exact: true })
      .click();
    await expect
      .poll(() => page.getByRole("alertdialog").elements().length)
      .toBe(1);
    await page.getByRole("button", { name: "确认", exact: true }).click();
    await expect
      .poll(async () =>
        (await getRequests())
          .filter((r) => r.path.endsWith("/git/actions"))
          .map((r) => r.body),
      )
      .toContainEqual({ action: "force_push" });
    await setScenario({
      git: {
        status: {
          root: "/tmp/demo",
          branch: "main",
          upstream: "origin/main",
          ahead: 0,
          behind: 0,
          remotes: ["origin"],
          files: [],
        },
        actionError: "Remote unavailable",
      },
    });
    await page
      .getByRole("button", { name: "Remote actions", exact: true })
      .click();
    await page.getByRole("menuitem", { name: "Fetch", exact: true }).click();
    await expect
      .poll(() => document.querySelector('[role="alert"]')?.textContent)
      .toContain("Remote unavailable");
  } finally {
    mounted.unmount();
  }
});
