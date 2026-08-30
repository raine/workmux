import type { Plugin } from '@opencode-ai/plugin';

export const WorkmuxStatusPlugin: Plugin = async ({ $ }) => {
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
  let reportedStatus: string | undefined;
  let statusQueue = Promise.resolve();

  function writeStatus(status: string) {
    return $`workmux set-window-status ${status}`.quiet().then(() => {}, () => {});
  }

  function queueStatus(status: string) {
    statusQueue = statusQueue.then(
      () => writeStatus(status),
      () => writeStatus(status),
    );
    return statusQueue;
  }

  async function reportAggregateStatus() {
    const statuses = [...statusBySession.values()];
    let status = 'done';

    if (statuses.includes('waiting')) {
      status = 'waiting';
    } else if (statuses.includes('working')) {
      status = 'working';
    }

    if (reportedStatus === status) {
      return;
    }

    reportedStatus = status;
    await queueStatus(status);
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
    event: async ({ event }) => {
      if (event.type === 'message.updated' && event.properties.info.role === 'user') {
        acceptBusyBySession.set(event.properties.sessionID, true);
      }

      switch (event.type) {
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
          if (statusBySession.delete(sessionID)) {
            await reportAggregateStatus();
          }
          break;
        }
      }
    },
  };
};
