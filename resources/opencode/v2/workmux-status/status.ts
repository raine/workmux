export type WindowStatus = 'working' | 'waiting' | 'done';

type SessionState = {
  status: WindowStatus;
  pending: Set<string>;
  acceptBusy: boolean;
};

type Hydration = {
  reset: Set<string>;
  replied: Map<string, Set<string>>;
};

export class StatusTracker {
  private readonly sessions = new Map<string, SessionState>();
  private lastStatus: WindowStatus | undefined;
  private readonly hydrations = new Set<Hydration>();

  constructor(private readonly report: (status: WindowStatus) => void) {}

  beginHydration() {
    const hydration: Hydration = { reset: new Set(), replied: new Map() };
    this.hydrations.add(hydration);
    return {
      seed: (sessionID: string, pending: readonly string[]) => {
        if (!this.hydrations.has(hydration) || hydration.reset.has(sessionID)) return;
        const replied = hydration.replied.get(sessionID);
        this.seed(sessionID, pending.filter((request) => !replied?.has(request)));
      },
      finish: () => {
        if (this.hydrations.delete(hydration)) this.update();
      },
    };
  }

  private resetHydration(sessionID: string) {
    // Execution boundaries and deletion supersede any in-flight snapshot.
    for (const hydration of this.hydrations) hydration.reset.add(sessionID);
  }

  seed(sessionID: string, pending: readonly string[] = []) {
    const state = this.sessions.get(sessionID);
    if (state) {
      if (!state.acceptBusy) return;
      for (const request of pending) state.pending.add(request);
      if (state.pending.size) state.status = 'waiting';
    } else {
      this.sessions.set(sessionID, {
        status: pending.length ? 'waiting' : 'working',
        pending: new Set(pending),
        acceptBusy: true,
      });
    }
    this.update();
  }

  start(sessionID: string) {
    this.resetHydration(sessionID);
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
    // A reply can arrive before the request's initial snapshot has loaded.
    for (const hydration of this.hydrations) {
      let replied = hydration.replied.get(sessionID);
      if (!replied) hydration.replied.set(sessionID, replied = new Set());
      replied.add(requestID);
    }
    const state = this.sessions.get(sessionID);
    if (!state?.acceptBusy || !state.pending.delete(requestID)) return;
    state.status = state.pending.size ? 'waiting' : 'working';
    this.update();
  }

  finish(sessionID: string) {
    this.resetHydration(sessionID);
    const state = this.sessions.get(sessionID);
    if (!state) return;
    state.pending.clear();
    state.status = 'done';
    state.acceptBusy = false;
    this.update();
  }

  forget(sessionID: string) {
    this.resetHydration(sessionID);
    if (this.sessions.delete(sessionID)) this.update();
  }

  isActive(sessionID: string): boolean {
    const status = this.sessions.get(sessionID)?.status;
    return status === 'working' || status === 'waiting';
  }

  private update() {
    if (!this.sessions.size && this.lastStatus === undefined) return;
    const statuses = [...this.sessions.values()].map((session) => session.status);
    const status: WindowStatus = statuses.includes('waiting')
      ? 'waiting'
      : statuses.includes('working')
        ? 'working'
        : 'done';
    // A not-yet-loaded child may still be running or waiting for input.
    if (status === 'done' && this.hydrations.size) return;
    if (status === this.lastStatus) return;
    this.lastStatus = status;
    this.report(status);
  }
}
