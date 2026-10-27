/**
 * Workmux status tracking extension for pi.
 *
 * Reports agent status to workmux for tmux window status display.
 * See: https://workmux.dev/guide/status-tracking
 */

import { spawn } from "node:child_process";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

type AssistantState = { stopReason?: string; errorMessage?: string };
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
  let sessionActive = false;
  let parentWorking = false;
  let activeCount: number | undefined;
  let unsubscribe: (() => void) | undefined;
  let writes = Promise.resolve();
  let lastStatus: string | undefined;
  let pendingPrompt: PendingPrompt | undefined;

  function setStatus(status: string) {
    const deduplicate = activeCount !== undefined;
    // A pending prompt rides the next `working` report, even a repeated one.
    const prompt = status === "working" ? pendingPrompt : undefined;
    if (prompt) pendingPrompt = undefined;
    // Event bus callbacks are not awaited by pi. Serialize external writes so an
    // older command cannot finish after a newer status and overwrite it.
    writes = writes.then(async () => {
      if (deduplicate && status === lastStatus && !prompt) return;
      const args = ["set-window-status", status];
      try {
        const code = prompt
          ? await execWithPrompt(args, prompt)
          : (await pi.exec("workmux", args)).code;
        lastStatus = code === 0 ? status : undefined;
      } catch {
        lastStatus = undefined;
      }
    });
    return writes;
  }

  function isWorking() {
    return parentWorking || (activeCount ?? 0) > 0;
  }

  function reportActivity() {
    return setStatus(isWorking() ? "working" : "done");
  }

  function latestAssistantWasAborted(ctx: ExtensionContext) {
    const branch = ctx.sessionManager.getBranch() as Array<{
      type?: string;
      message?: AssistantState & { role?: string };
    }>;
    for (let index = branch.length - 1; index >= 0; index--) {
      const entry = branch[index];
      if (entry?.type !== "message" || entry.message?.role !== "assistant") {
        continue;
      }
      return (
        entry.message.stopReason === "aborted" ||
        (entry.message.stopReason === "error" &&
          /\boperation was aborted\b/i.test(entry.message.errorMessage ?? ""))
      );
    }
    return false;
  }

  pi.on("session_start", async (_event, ctx) => {
    unsubscribe?.();
    sessionActive = false;
    await writes;
    activeCount = undefined;
    lastStatus = undefined;
    pendingPrompt = undefined;
    parentWorking = !ctx.isIdle();
    await pi.exec("workmux", ["register-agent"]).catch(() => {});
    sessionActive = true;
    unsubscribe = pi.events.on("suba:activity", (data) => {
      if (!sessionActive || !data || typeof data !== "object") return;
      const count = (data as { activeCount?: unknown }).activeCount;
      if (typeof count !== "number" || !Number.isSafeInteger(count) || count < 0) return;
      activeCount = count;
      return reportActivity();
    });
    // The publisher may have restored children before this handler subscribed.
    pi.events.emit("suba:activity:request", {});
    await writes;
  });

  pi.on("input", async (event, ctx) => {
    if (!sessionActive || event.source === "extension" || !event.text.trim()) return;
    pendingPrompt = { text: event.text, cwd: ctx.cwd };
    // Steering and follow-up messages are handled inside the running agent
    // loop, which starts no new `agent_start`.
    if (isWorking()) await setStatus("working");
  });

  pi.on("agent_start", async () => {
    if (!sessionActive) return;
    parentWorking = true;
    await setStatus("working");
  });

  pi.on("agent_settled", async (_event, ctx) => {
    if (!sessionActive) return;
    const wasAborted = latestAssistantWasAborted(ctx);
    parentWorking = !ctx.isIdle() || wasAborted;
    if (activeCount !== undefined) {
      await reportActivity();
    } else if (!wasAborted) {
      await setStatus("done");
    }
  });

  pi.on("session_shutdown", async () => {
    unsubscribe?.();
    unsubscribe = undefined;
    const wasActive = sessionActive;
    sessionActive = false;
    if (wasActive && activeCount !== undefined) await setStatus("done");
    await writes;
  });
}
