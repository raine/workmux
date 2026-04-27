/**
 * Workmux status tracking extension for pi.
 *
 * Reports agent status to workmux for tmux window status display.
 * See: https://workmux.raine.dev/guide/status-tracking
 */

import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  function setStatus(status: string) {
    pi.exec("workmux", ["set-window-status", status]).catch(() => {});
  }

  pi.on("agent_start", async () => {
    setStatus("working");
  });

  pi.on("agent_end", async () => {
    setStatus("done");
  });
}
