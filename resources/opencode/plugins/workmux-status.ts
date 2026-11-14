import type { Plugin } from '@opencode-ai/plugin';

export const WorkmuxStatusPlugin: Plugin = async ({ $, client }) => {
  try {
    await $`workmux register-agent`.quiet();
  } catch {
    // Status tracking remains available when registration cannot reach workmux.
  }

  // OpenCode can emit repeated `session.status busy` events for a single turn,
  // and can even emit a stale trailing `busy` after `idle` at the end. Track
  // every parent and child session so one idle session cannot mark the whole
  // pane done while another session is still working.
  const statusBySession = new Map<string, string>();
  const acceptBusyBySession = new Map<string, boolean>();
  const deletedSessions = new Set<string>();
  // Subagent task sessions also receive user messages; only top-level
  // sessions carry the user's own prompt.
  const childBySession = new Map<string, boolean>();
  let reportedStatus: string | undefined;
  let pendingPrompt: string | undefined;
  let statusQueue = Promise.resolve();

  function writeStatus(status: string, prompt?: string) {
    const command = prompt === undefined
      ? $`workmux set-window-status ${status}`
      : $`workmux set-window-status ${status} < ${new Response(JSON.stringify({ prompt }))}`;
    return command.quiet().then(() => {}, () => {});
  }

  function queueStatus(status: string, prompt?: string) {
    statusQueue = statusQueue.then(
      () => writeStatus(status, prompt),
      () => writeStatus(status, prompt),
    );
    return statusQueue;
  }

  // A pending prompt rides the next `working` report, even a repeated one.
  function takePrompt(status: string) {
    if (status !== 'working') {
      return undefined;
    }
    const prompt = pendingPrompt;
    pendingPrompt = undefined;
    return prompt;
  }

  async function isChildSession(sessionID: string) {
    const known = childBySession.get(sessionID);
    if (known !== undefined) {
      return known;
    }
    try {
      const result = await client.session.get({ path: { id: sessionID } });
      if (!result.data) {
        return true;
      }
      const isChild = Boolean(result.data.parentID);
      childBySession.set(sessionID, isChild);
      return isChild;
    } catch {
      // An unclassified session may be a subagent; skip its prompt.
      return true;
    }
  }

  async function reportAggregateStatus() {
    const statuses = [...statusBySession.values()];
    let status = 'done';

    if (statuses.includes('waiting')) {
      status = 'waiting';
    } else if (statuses.includes('working')) {
      status = 'working';
    }

    if (reportedStatus === status && !(status === 'working' && pendingPrompt !== undefined)) {
      return;
    }

    reportedStatus = status;
    await queueStatus(status, takePrompt(status));
  }

  async function setStatus(
    sessionID: string | undefined,
    status: string,
  ) {
    if (!sessionID || deletedSessions.has(sessionID)) {
      return;
    }

    const previous = statusBySession.get(sessionID);
    if (status === 'done' && previous === undefined) {
      return;
    }
    // Ignore the final stale `busy` OpenCode sometimes emits after a session is
    // already done. The next user message re-arms `working` for the new turn.
    if (status === 'working' && acceptBusyBySession.get(sessionID) === false) {
      return;
    }
    if (previous === status) {
      return;
    }

    statusBySession.set(sessionID, status);
    if (status === 'done') {
      acceptBusyBySession.set(sessionID, false);
    } else {
      acceptBusyBySession.set(sessionID, true);
    }

    await reportAggregateStatus();
  }

  return {
    'chat.message': async ({ sessionID }, { parts }) => {
      if (await isChildSession(sessionID)) {
        return;
      }
      const prompt = parts
        .flatMap((part) => (part.type === 'text' && !part.synthetic ? [part.text] : []))
        .join('\n');
      if (!prompt.trim()) {
        return;
      }
      pendingPrompt = prompt;
      // A message queued while the agent works starts no new busy transition.
      if (reportedStatus === 'working') {
        await queueStatus('working', takePrompt('working'));
      }
    },
    event: async ({ event }) => {
      if (event.type === 'message.updated' && event.properties.info.role === 'user') {
        acceptBusyBySession.set(event.properties.sessionID, true);
      }

      switch (event.type) {
        case 'session.created':
          childBySession.set(event.properties.info.id, Boolean(event.properties.info.parentID));
          break;
        case 'session.status':
          if (event.properties.status.type === 'busy') {
            await setStatus(event.properties.sessionID, 'working');
          }
          if (event.properties.status.type === 'idle') {
            await setStatus(event.properties.sessionID, 'done');
          }
          break;
        case 'permission.asked':
        case 'question.asked':
          await setStatus(event.properties.sessionID, 'waiting');
          break;
        case 'permission.replied':
        case 'question.replied':
          await setStatus(event.properties.sessionID, 'working');
          break;
        case 'session.idle':
          await setStatus(event.properties.sessionID, 'done');
          break;
        case 'session.deleted': {
          const sessionID = event.properties.info.id;
          deletedSessions.add(sessionID);
          acceptBusyBySession.delete(sessionID);
          childBySession.delete(sessionID);
          if (statusBySession.delete(sessionID)) {
            await reportAggregateStatus();
          }
          break;
        }
      }
    },
  };
};
