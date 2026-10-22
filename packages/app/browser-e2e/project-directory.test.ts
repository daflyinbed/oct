import { afterEach, expect, it } from "vitest";
import { page, userEvent } from "vitest/browser";
import client from "@/api/client";
import { mountApp } from "./support/app";
import { getRequests, resetMock, setScenario } from "./support/mock";
import type { MountedApp } from "./support/app";
import type { components } from "@/api/schema";
import type { Middleware } from "openapi-fetch";

type Listing = components["schemas"]["DirectoryListing"];

function listing(directory: string, names: string[], query = ""): Listing {
  return {
    directory,
    parent: directory.slice(0, directory.lastIndexOf("/")) || "/",
    query,
    entries: names.map((name) => ({ name, path: `${directory}/${name}` })),
  };
}

let mounted: MountedApp | undefined;
afterEach(() => {
  mounted?.unmount();
  mounted = undefined;
});

async function open() {
  await resetMock();
  const home = listing("/home/me", ["Documents", ".docker", "ZCodeProject"]);
  await setScenario({
    projects: [],
    directoryReplies: {
      "": { body: home },
      "/home/me/": { body: home },
      "/home/me/Doc": { body: { ...home, query: "Doc" } },
      "/home/me/Documents/": { body: listing("/home/me/Documents", ["oct"]) },
      "/home/me/Documents/oct/": {
        body: listing("/home/me/Documents/oct", [".git", "crates", "docs"]),
      },
      "/home/me/nomatch": { body: listing("/home/me", [], "nomatch") },
      "/opt/shared space/": { body: listing("/opt/shared space", []) },
      "/slow/": { body: listing("/slow", ["outdated"]) },
    },
  });
  mounted = await mountApp();
  await page.getByTitle("菜单", { exact: true }).click();
  await page
    .getByRole("menuitem", { name: "New Project…", exact: true })
    .click();
  await expect.element(page.getByRole("dialog")).toBeVisible();
}

function directory() {
  return page.getByRole("combobox", { name: "Working directory", exact: true });
}
async function pathValue() {
  return ((await directory().element()) as HTMLInputElement).value;
}
function options() {
  return [...document.querySelectorAll('[role="option"]')].map(
    (el) => el.textContent,
  );
}
async function creates() {
  return (await getRequests()).filter(
    (req) => req.method === "POST" && req.path === "/api/projects",
  );
}

it("completes a path, browses with Tab and Enter, and creates a project only after confirmation", async () => {
  await open();
  await directory().click();
  await expect.poll(pathValue).toBe("/home/me/");
  await directory().fill("/home/me/Doc");
  await expect.poll(options).toEqual(["Documents", ".docker", "ZCodeProject"]);
  await userEvent.keyboard("{ArrowDown}");
  await expect
    .poll(async () => {
      const id = (await directory().element()).getAttribute(
        "aria-activedescendant",
      );
      return id
        ? document.querySelector(`#${CSS.escape(id)}`)?.textContent
        : null;
    })
    .toBe(".docker");
  await userEvent.keyboard("{ArrowUp}");
  await userEvent.keyboard("{Tab}");
  await expect.poll(pathValue).toBe("/home/me/Documents/");
  await expect.poll(options).toContain("oct");
  await page.getByRole("option", { name: "oct", exact: true }).click();
  await expect.poll(pathValue).toBe("/home/me/Documents/oct/");
  await expect.poll(options).toContain(".git");
  await userEvent.keyboard("{Enter}");
  await expect.poll(pathValue).toBe("/home/me/Documents/oct");
  await expect.poll(options).toEqual([]);
  expect(await creates()).toEqual([]);
  await expect
    .element(page.getByRole("textbox", { name: "Project name" }))
    .toHaveValue("oct");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect
    .poll(async () => (await creates())[0]?.body)
    .toEqual({
      name: "oct",
      working_dir: "/home/me/Documents/oct",
    });
});

it("supports parent navigation, empty/error states and choosing additional directories", async () => {
  await open();
  await directory().fill("/home/me/Documents/oct/");
  await expect.poll(options).toContain("crates");
  await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
  await expect.poll(pathValue).toBe("/home/me/Documents/");
  await expect.poll(options).toContain("oct");
  await page.getByRole("button", { name: "返回上级目录", exact: true }).click();
  await expect.poll(pathValue).toBe("/home/me/");
  await directory().fill("/home/me/nomatch");
  await expect
    .element(page.getByText("没有匹配的目录", { exact: true }))
    .toBeVisible();
  await directory().fill("/missing/");
  await expect
    .poll(async () => (await page.getByRole("alert").element()).textContent)
    .toContain("无法读取目录");
  await userEvent.keyboard("{Enter}");
  expect(await creates()).toEqual([]);
  await userEvent.keyboard("{Escape}");
  await expect.element(page.getByRole("dialog")).toBeVisible();

  await page.getByRole("textbox", { name: "Project name" }).fill("custom name");
  await directory().fill("/home/me/Documents/oct/");
  await expect.poll(options).toContain("docs");
  await page.getByRole("option", { name: /使用此目录/ }).click();
  await expect
    .element(page.getByRole("textbox", { name: "Project name" }))
    .toHaveValue("custom name");
  await page.getByRole("button", { name: "+ 添加目录", exact: true }).click();
  const additional = page.getByRole("combobox", {
    name: "Additional directory 1",
    exact: true,
  });
  await additional.fill("/opt/shared space/");
  await expect
    .element(page.getByText("此目录下没有子目录", { exact: true }))
    .toBeVisible();
  await page.getByRole("option", { name: /使用此目录/ }).click();
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect
    .poll(async () => (await creates())[0]?.body)
    .toEqual({
      name: "custom name",
      working_dir: "/home/me/Documents/oct",
      additional_dirs: ["/opt/shared space"],
    });
});

it("discards a slow response after the user has changed the path", async () => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let blocked = false;
  let released = false;
  const slow: Middleware = {
    async onResponse({ request, response }) {
      const url = new URL(request.url);
      if (
        url.pathname === "/api/directories" &&
        url.searchParams.get("path") === "/slow/"
      ) {
        blocked = true;
        await gate;
        released = true;
      }
      return response;
    },
  };
  client.use(slow);
  try {
    await open();
    await directory().fill("/slow/");
    await expect.poll(() => blocked).toBe(true);
    await directory().fill("/home/me/Documents/oct/");
    await expect.poll(options).toContain("crates");
    release();
    await expect.poll(() => released).toBe(true);
    await expect.poll(options).toContain("crates");
    expect(options()).not.toContain("outdated");
  } finally {
    release();
    client.eject(slow);
  }
});
