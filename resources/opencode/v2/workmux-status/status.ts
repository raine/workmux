export type WindowStatus = 'working' | 'waiting' | 'done';

export type SessionState = {
  running: boolean;
  pending: Set<string>;
};

export type SessionEvent = {
  sessionID: string;
  parentID?: string;
} & (
  | { type: 'created' | 'started' | 'finished' | 'deleted' }
  | { type: 'asked' | 'replied'; requestID: string }
);

export function sessionState(running = false, pending: readonly string[] = []): SessionState {
  return { running, pending: new Set(pending) };
}

export function applyEvent(sessions: Map<string, SessionState>, event: SessionEvent) {
  if (event.type === 'deleted') {
    sessions.delete(event.sessionID);
    return;
  }
  let state = sessions.get(event.sessionID);
  if (!state) sessions.set(event.sessionID, state = sessionState());
  switch (event.type) {
    case 'started':
      state.running = true;
      break;
    case 'finished':
      state.running = false;
      state.pending.clear();
      break;
    case 'asked':
      state.pending.add(event.requestID);
      break;
    case 'replied':
      state.pending.delete(event.requestID);
      break;
  }
}

export function isActive(state: SessionState): boolean {
  return state.running || state.pending.size > 0;
}

export function windowStatus(sessions: Iterable<SessionState>): WindowStatus {
  let running = false;
  for (const state of sessions) {
    if (state.pending.size) return 'waiting';
    running ||= state.running;
  }
  return running ? 'working' : 'done';
}
