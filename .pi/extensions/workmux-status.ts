/**
 * Workmux status tracking extension for pi.
 *
 * Reports agent status to workmux for tmux window status display.
 * See: https://workmux.raine.dev/guide/status-tracking
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  function setStatus(status: string) {
    pi.exec("workmux", ["set-window-status", status]).catch(() => {});
  }

  function lastAssistantText(messages: any[]): string {
    for (let i = messages.length - 1; i >= 0; i--) {
      const m = messages[i];
      if (m?.role === "assistant" && Array.isArray(m.content)) {
        const text = m.content
          .filter((b: any) => b?.type === "text")
          .map((b: any) => b.text ?? "")
          .join("")
          .trim();
        if (text) return text;
      }
    }
    return "";
  }

  pi.on("agent_start", async () => {
    setStatus("working");
  });

  pi.on("agent_end", async (event: any) => {
    setStatus("done");

    const summary = lastAssistantText(event?.messages ?? []);
    const args = summary
      ? ["notify", "--body", summary.slice(0, 200)]
      : ["notify"];
    // `workmux notify` blocks until the notification is clicked/dismissed,
    // so spawn it fully detached so it survives independent of pi's lifecycle.
    // If we used pi.exec (which captures output), pi could tear down the child
    // and close the D-Bus connection, dismissing the notification.
    try {
      const { spawn } = await import("node:child_process");
      const child = spawn("workmux", args, {
        detached: true,
        stdio: "ignore",
      });
      child.unref();
    } catch (e) {
      console.error("[workmux] failed to spawn notify:", e);
    }
  });
}
