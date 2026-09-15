import { readFile } from "node:fs/promises";
import { test, expect } from "@playwright/test";
import { createTerminal, openReturningWorkspace } from "./workspace-auth";

for (const connection of ["local", "test"])
  test(`keeps the terminal canvas mounted through reconnect with ${connection} attached`, async ({
    page,
  }, testInfo) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.addInitScript(() => {
      const sockets = new Set<WebSocket>();
      const NativeWebSocket = window.WebSocket;
      window.WebSocket = class extends NativeWebSocket {
        constructor(url: string | URL, protocols?: string | string[]) {
          super(url, protocols);
          if (new URL(url, location.href).pathname === "/edge") {
            sockets.add(this);
            this.addEventListener("close", () => sockets.delete(this));
          }
        }
      };
      Object.assign(window, {
        disconnectYasForTest: () => {
          const count = sockets.size;
          for (const socket of sockets) socket.close();
          return count;
        },
      });
    });
    let holdReplies = false;
    const replies: Array<() => void> = [];
    await page.routeWebSocket("**/edge", (socket) => {
      const server = socket.connectToServer();
      server.onMessage((message) => {
        if (holdReplies && message === "ok")
          replies.push(() => socket.send(message));
        else socket.send(message);
      });
    });
    await openReturningWorkspace(page);
    await expect(page.getByRole("status", { name: "Connected" })).toBeVisible();
    await createTerminal(page);
    if (connection === "test") {
      await page.getByRole("status").click();
      const remote = page
        .getByRole("dialog")
        .getByRole("listitem")
        .filter({ hasText: "test" });
      await remote
        .getByRole("checkbox", { name: "Add to workspace", exact: true })
        .check();
      await expect(remote.locator('[title="Connected"]')).toBeVisible();
      await page.keyboard.press("Escape");
    }
    await page.locator('[data-yas-pane-focused="true"]').click();
    const inputName = await page
      .locator('textarea[aria-label="Terminal input"]:focus')
      .getAttribute("name");
    const input = page.locator(`textarea[name="${inputName}"]`);
    await expect(input).toBeFocused();
    const canvases = await page.locator("canvas:visible").elementHandles();
    expect(canvases.length).toBeGreaterThan(0);
    const originalInput = await input.elementHandle();

    holdReplies = true;
    const disconnected = await page.evaluate(() => {
      return (
        window as unknown as { disconnectYasForTest(): number }
      ).disconnectYasForTest();
    });
    expect(disconnected).toBeGreaterThan(0);
    // Hold the next authentication reply so the disconnected presentation can
    // be checked deterministically, regardless of the machine's retry speed.
    await expect.poll(() => replies.length).toBeGreaterThan(0);
    if (connection === "test") {
      // Both connections remain represented, even if their recovery states
      // differ. The badge shows one count per state; RTT is separate.
      const counts = await page
        .getByRole("status")
        .locator(":scope > span:not([data-yas-connection-rtt])")
        .allTextContents();
      expect(counts.reduce((sum, count) => sum + Number(count), 0)).toBe(2);
    }
    for (const canvas of canvases)
      expect(await canvas.evaluate((element) => element.isConnected)).toBe(
        true,
      );
    expect(
      await originalInput!.evaluate((element) => element.isConnected),
    ).toBe(true);

    holdReplies = false;
    for (const send of replies.splice(0)) send();
    await expect(page.getByRole("status", { name: "Connected" })).toBeVisible();
    await expect(input).toBeFocused();
    // Both fixture routes lead to this isolated local server. Prove input is
    // delivered after the new view opens, then check the original DOM again.
    const marker = testInfo.outputPath("reconnected.txt");
    await page.keyboard.press("Control+c");
    await page.keyboard.insertText(
      `printf YAS_RECONNECTED > '${marker.replace(/'/g, "'\\''")}'`,
    );
    await page.keyboard.press("Enter");
    await expect
      .poll(() => readFile(marker, "utf8").catch(() => ""))
      .toBe("YAS_RECONNECTED");
    for (const canvas of canvases)
      expect(await canvas.evaluate((element) => element.isConnected)).toBe(
        true,
      );
    expect(
      await originalInput!.evaluate((element) => element.isConnected),
    ).toBe(true);
    await expect(input).toBeFocused();
    expect(errors).toEqual([]);
  });
