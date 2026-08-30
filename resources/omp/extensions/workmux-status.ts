/**
 * Workmux status tracking extension for oh-my-pi.
 *
 * Reports agent status to workmux for tmux window status display.
 * See: https://workmux.raine.dev/guide/status-tracking
 */

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  let lastStatus: string | undefined;
  let statusQueue = Promise.resolve();

  function writeStatus(status: string) {
    return pi.exec("workmux", ["set-window-status", status]).then(() => {}, () => {});
  }

  function setStatus(status: string) {
    if (status === lastStatus) {
      return statusQueue;
    }
    lastStatus = status;
    statusQueue = statusQueue.then(
      () => writeStatus(status),
      () => writeStatus(status),
    );
    return statusQueue;
  }

  pi.on("session_start", async () => {
    await pi.exec("workmux", ["register-agent"]).catch(() => {});
  });

  pi.on("agent_start", async () => {
    await setStatus("working");
  });

  pi.on("message_end", async (event) => {
    if ("role" in event.message && event.message.role === "assistant") {
      await setStatus("waiting");
    }
  });

  pi.on("tool_call", async (event) => {
    if (event.toolName === "ask") {
      await setStatus("waiting");
    } else {
      await setStatus("working");
    }
  });

  pi.on("tool_execution_start", async () => {
    await setStatus("working");
  });

  pi.on("agent_end", async () => {
    if (lastStatus === "done") {
      return;
    }
    lastStatus = "done";
    await writeStatus("done");
  });
}
