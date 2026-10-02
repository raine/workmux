export type WindowStatus = 'working' | 'waiting' | 'done';

type SessionState = {
  status: WindowStatus;
  pending: Set<string>;
  acceptBusy: boolean;
};

export class StatusTracker {
  private readonly sessions = new Map<string, SessionState>();
  private lastStatus: WindowStatus | undefined;

  constructor(private readonly report: (status: WindowStatus) => void) {}

  seed(sessionID: string, pending: readonly string[] = []) {
    if (this.sessions.has(sessionID)) return;
    this.sessions.set(sessionID, {
      status: pending.length ? 'waiting' : 'working',
      pending: new Set(pending),
      acceptBusy: true,
    });
    this.update();
  }

  start(sessionID: string) {
    const state = this.sessions.get(sessionID);
    if (state) {
      state.pending.clear();
      state.status = 'working';
      state.acceptBusy = true;
    } else {
      this.seed(sessionID);
    }
    this.update();
  }

  busy(sessionID: string) {
    const state = this.sessions.get(sessionID);
    if (!state) return this.seed(sessionID);
    if (!state.acceptBusy) return;
    state.status = state.pending.size ? 'waiting' : 'working';
    this.update();
  }

  wait(sessionID: string, requestID: string) {
    const state = this.sessions.get(sessionID);
    if (state && !state.acceptBusy) return;
    if (!state) {
      this.seed(sessionID, [requestID]);
      return;
    }
    const current = state;
    current.pending.add(requestID);
    current.status = 'waiting';
    this.update();
  }

  resume(sessionID: string, requestID: string) {
    const state = this.sessions.get(sessionID);
    if (!state?.acceptBusy || !state.pending.delete(requestID)) return;
    state.status = state.pending.size ? 'waiting' : 'working';
    this.update();
  }

  finish(sessionID: string) {
    const state = this.sessions.get(sessionID);
    if (!state) return;
    state.pending.clear();
    state.status = 'done';
    state.acceptBusy = false;
    this.update();
  }

  forget(sessionID: string) {
    if (this.sessions.delete(sessionID)) this.update();
  }

  private update() {
    const statuses = [...this.sessions.values()].map((session) => session.status);
    const status: WindowStatus = statuses.includes('waiting')
      ? 'waiting'
      : statuses.includes('working')
        ? 'working'
        : 'done';
    if (status === this.lastStatus) return;
    this.lastStatus = status;
    this.report(status);
  }
}
