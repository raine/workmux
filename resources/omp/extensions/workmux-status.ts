/**
 * Workmux status tracking extension for oh-my-pi.
 *
 * Reports agent status to workmux for tmux window status display.
 * See: https://workmux.dev/guide/status-tracking
 */

import { spawn } from "node:child_process";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

type PendingPrompt = { text: string; cwd: string };

// `pi.exec` cannot write to stdin, so prompt-carrying reports spawn directly.
// Like `pi.exec`, this resolves with an exit code and never throws.
function execWithPrompt(args: string[], prompt: PendingPrompt): Promise<number> {
  return new Promise((resolve) => {
    try {
      const child = spawn("workmux", args, {
        cwd: prompt.cwd,
        shell: false,
        stdio: ["pipe", "ignore", "ignore"],
      });
      child.on("error", () => resolve(1));
      child.on("close", (code) => resolve(code ?? 1));
      child.stdin.on("error", () => {});
      child.stdin.end(JSON.stringify({ prompt: prompt.text }));
    } catch {
      resolve(1);
    }
  });
}

export default function (pi: ExtensionAPI) {
  let lastStatus: string | undefined;
  let pendingPrompt: PendingPrompt | undefined;
  let statusQueue = Promise.resolve();

  function writeStatus(status: string, prompt?: PendingPrompt) {
    const args = ["set-window-status", status];
    const write = prompt ? execWithPrompt(args, prompt) : pi.exec("workmux", args);
    return write.then(() => {}, () => {});
  }

  function setStatus(status: string) {
    // A pending prompt rides the next `working` report, even a repeated one.
    const prompt = status === "working" ? pendingPrompt : undefined;
    if (status === lastStatus && !prompt) {
      return statusQueue;
    }
    lastStatus = status;
    if (prompt) pendingPrompt = undefined;
    statusQueue = statusQueue.then(
      () => writeStatus(status, prompt),
      () => writeStatus(status, prompt),
    );
    return statusQueue;
  }

  pi.on("session_start", async () => {
    pendingPrompt = undefined;
    await pi.exec("workmux", ["register-agent"]).catch(() => {});
  });

  pi.on("input", async (event, ctx) => {
    const text = event.text.trimStart();
    // Input handlers run before oh-my-pi handles `!` shell and `$` Python
    // input, which never reach the agent.
    if (event.source === "extension" || !text || text.startsWith("!") || text.startsWith("$")) {
      return;
    }
    pendingPrompt = { text: event.text, cwd: ctx.cwd };
    // Steering messages are handled inside the running agent loop, which
    // starts no new `agent_start`. Built-in slash commands also arrive here,
    // so they wait for a turn that actually starts.
    if (lastStatus === "working" && !text.startsWith("/")) {
      await setStatus("working");
    }
  });

  pi.on("agent_start", async () => {
    await setStatus("working");
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
    // Input that started no turn, such as a built-in command, is not a prompt.
    pendingPrompt = undefined;
    await setStatus("done");
  });
}
