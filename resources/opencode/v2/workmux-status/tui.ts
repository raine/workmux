import { spawn } from 'node:child_process';
import { Plugin } from '@opencode/plugin/tui';
import { applyEvent, isActive, sessionState, windowStatus, type SessionEvent, type SessionState, type WindowStatus } from './status';

function hasLocalPane(): boolean {
  // Match workmux's backend selection, but require an identifiable local pane.
  const panes: Record<string, boolean> = {
    tmux: !!process.env.TMUX && !!process.env.TMUX_PANE,
    wezterm: !!process.env.WEZTERM_PANE,
    zellij: !!process.env.ZELLIJ_PANE_ID,
    kitty: !!process.env.KITTY_WINDOW_ID,
  };
  const override = process.env.WORKMUX_BACKEND;
  if (override) return panes[override] ?? false;
  if (process.env.TMUX !== undefined) return panes.tmux;
  if (process.env.WEZTERM_PANE) return panes.wezterm;
  if (process.env.ZELLIJ || process.env.ZELLIJ_PANE_ID || process.env.ZELLIJ_SESSION_NAME) return panes.zellij;
  return panes.kitty;
}

function isMissingSession(error: unknown, sessionID: string): boolean {
  return typeof error === 'object' && error !== null
    && '_tag' in error && error._tag === 'SessionNotFoundError'
    && 'sessionID' in error && error.sessionID === sessionID;
}

function workmux(args: string[], prompt?: string): Promise<boolean> {
  return new Promise((resolve) => {
    // Workmux reads hook metadata until EOF; close stdin after sending a prompt.
    const child = spawn('workmux', args, { stdio: [prompt === undefined ? 'ignore' : 'pipe', 'ignore', 'ignore'], timeout: 3000 });
    if (child.stdin) {
      child.stdin.on('error', () => {});
      child.stdin.end(JSON.stringify({ prompt }));
    }
    let spawnError: string | undefined;
    child.on('error', (error: NodeJS.ErrnoException) => {
      spawnError = error.code ?? 'spawn error';
    });
    child.on('close', (code, signal) => {
      if (code === 0 && !spawnError) return resolve(true);
      console.warn(`[workmux.status] workmux ${args.join(' ')} failed (${spawnError ?? signal ?? `exit ${code}`})`);
      resolve(false);
    });
  });
}

export default Plugin.define({
  id: 'workmux.status',
  setup(ctx) {
    // The shared server has no reliable relationship to this terminal pane.
    if (!hasLocalPane()) return;

    let closed = false;
    let desired: WindowStatus | undefined;
    let reported: WindowStatus | undefined;
    let queue = workmux(['register-agent']).then(() => {});
    let pendingPrompt: { text: string; sessionID: string } | undefined;
    let promptVersion = 0;
    let capturedPromptVersion = 0;

    type OwnedSession = SessionState & { idle?: number | null };
    type Snapshot = { events: SessionEvent[]; inFlight: boolean; retryAt: number };
    type Family = { sessions: Map<string, OwnedSession>; snapshot?: Snapshot };
    const families = new Map<string, Family>();
    const deleted = new Set<string>();
    let visibleRoot: string | undefined;

    function publish() {
      for (const [root, family] of families) {
        if (root !== visibleRoot && !family.snapshot && ![...family.sessions.values()].some(isActive)) {
          families.delete(root);
        }
      }
      const status = windowStatus([...families.values()].flatMap((family) => [...family.sessions.values()]));
      if (status === 'done' && (desired === undefined || [...families.values()].some((family) => family.snapshot))) return;
      if (pendingPrompt && !owner(pendingPrompt.sessionID)) pendingPrompt = undefined;
      if (status === desired && status === reported && !(status === 'working' && pendingPrompt)) return;
      desired = status;
      queue = queue.then(async () => {
        if (closed || !desired) return;
        const next = desired;
        const prompt = next === 'working' ? pendingPrompt : undefined;
        if (next === reported && !prompt) return;
        if (await workmux(['set-window-status', next], prompt?.text)) {
          reported = next;
          if (pendingPrompt === prompt) pendingPrompt = undefined;
        }
      });
    }

    function owner(sessionID: string, parentID?: string): Family | undefined {
      if (deleted.has(sessionID)) return;
      for (const family of families.values()) {
        if (family.sessions.has(sessionID) || (parentID && family.sessions.has(parentID))) return family;
      }
      return families.get(ctx.data.session.root(parentID ?? sessionID));
    }

    async function capturePrompt(sessionID: string, text: string) {
      if (!text.trim() || !owner(sessionID)) return;
      const version = ++promptVersion;
      try {
        const info = ctx.data.session.get(sessionID) ?? await ctx.client.session.get({ sessionID });
        if (closed || version < capturedPromptVersion || info.parentID || !owner(sessionID)) return;
        capturedPromptVersion = version;
        pendingPrompt = { text, sessionID };
        publish();
      } catch {
        // An unclassified session may be a subagent; skip its prompt.
      }
    }

    function observe(family: Family, event: SessionEvent) {
      applyEvent(family.sessions, event);
      const state = family.sessions.get(event.sessionID);
      const info = ctx.data.session.get(event.sessionID);
      if (state && !('idle' in state) && info) state.idle = info.time?.idle ?? null;
    }

    function moveFamily(root: string, nextRoot: string, family: Family) {
      const target = families.get(nextRoot) ?? family;
      for (const [id, state] of family.sessions) {
        if (!target.sessions.has(id)) target.sessions.set(id, state);
      }
      families.delete(root);
      families.set(nextRoot, target);
      target.snapshot = { events: [], inFlight: false, retryAt: 0 };
      if (visibleRoot === root) visibleRoot = nextRoot;
      void refresh(nextRoot, target);
    }

    async function refresh(root: string, family: Family) {
      const snapshot = family.snapshot;
      if (!snapshot || snapshot.inFlight || snapshot.retryAt > Date.now()) return;
      snapshot.inFlight = true;
      snapshot.events = [];
      const current = () => !closed && families.get(root) === family && family.snapshot === snapshot;
      try {
        await ctx.data.session.sync(root);
        if (!current()) return;
        const resolvedRoot = ctx.data.session.root(root);
        if (resolvedRoot !== root) {
          moveFamily(root, resolvedRoot, family);
          return;
        }
        const rootInfo = ctx.data.session.get(root);
        const active = await ctx.client.session.active();
        if (!current()) return;
        const sessions = new Map<string, OwnedSession>([[root, sessionState()]]);
        const metadata = new Map([[root, rootInfo]]);
        // parentID selects direct children; page and recurse to include the whole
        // family, including descendants that have never entered the TUI cache.
        for (const sessionID of sessions.keys()) {
          let cursor: string | undefined;
          do {
            const page = await ctx.client.session.list({ parentID: sessionID, order: 'desc', cursor });
            if (!current()) return;
            for (const child of page.data) {
              if (!deleted.has(child.id)) {
                sessions.set(child.id, sessionState());
                metadata.set(child.id, child);
              }
            }
            cursor = page.cursor.next ?? undefined;
          } while (cursor);
        }
        for (const [sessionID, state] of sessions) {
          const requests = await Promise.all([
            ctx.client.permission.list({ sessionID }),
            ctx.client.session.form.list({ sessionID }),
          ]).catch((error: unknown) => {
            if (sessionID !== root && isMissingSession(error, sessionID)) return;
            throw error;
          });
          if (!requests) {
            sessions.delete(sessionID);
            continue;
          }
          const [permissions, forms] = requests;
          if (!current()) return;
          state.running = sessionID in active;
          state.pending = new Set([
            ...permissions.map((request) => `permission:${request.id}`),
            ...forms.map((form) => `form:${form.id}`),
          ]);
          const info = metadata.get(sessionID);
          if (info) state.idle = info.time?.idle ?? null;
          const previous = family.sessions.get(sessionID);
          // A terminal marker advancing while disconnected means this session's
          // retained execution ended; a later execution belongs to another view.
          if (root !== visibleRoot && previous && 'idle' in previous && state.idle != null
              && state.idle > (previous.idle ?? 0)) {
            state.running = false;
            state.pending.clear();
          }
        }
        // Events may precede descendant discovery. Buffer them before ownership
        // filtering and replay only those that belong to the discovered family.
        for (const event of snapshot.events) {
          if (sessions.has(event.sessionID) || (event.parentID && sessions.has(event.parentID))) {
            applyEvent(sessions, event);
          }
        }
        if (!current()) return;
        family.sessions = sessions;
        family.snapshot = undefined;
        publish();
      } catch (error) {
        if (!current()) return;
        if (isMissingSession(error, root)) {
          deleted.add(root);
          families.delete(root);
          publish();
          return;
        }
        snapshot.inFlight = false;
        snapshot.retryAt = Date.now() + 1000;
        console.warn('[workmux.status] could not refresh session status', error);
      }
    }

    function captureLocalSession() {
      const route = ctx.ui.router.current();
      const sessionID = route.type === 'session' ? route.sessionID : undefined;
      visibleRoot = sessionID && !deleted.has(sessionID) ? ctx.data.session.root(sessionID) : undefined;
      if (visibleRoot && deleted.has(visibleRoot)) visibleRoot = undefined;
      if (visibleRoot) {
        for (const [root, family] of families) {
          if (root !== visibleRoot && ctx.data.session.root(root) === visibleRoot) moveFamily(root, visibleRoot, family);
        }
      }
      if (visibleRoot && !families.has(visibleRoot)) {
        const sessions = new Map<string, OwnedSession>();
        for (const id of new Set([visibleRoot, sessionID!, ...ctx.data.session.family(visibleRoot)])) {
          if (deleted.has(id)) continue;
          const info = ctx.data.session.get(id);
          sessions.set(id, {
            ...sessionState(ctx.data.session.status(id) === 'running'),
            ...(info ? { idle: info.time?.idle ?? null } : {}),
          });
        }
        families.set(visibleRoot, { sessions, snapshot: { events: [], inFlight: false, retryAt: 0 } });
      }
      for (const [root, family] of families) void refresh(root, family);
      publish();
    }

    const stop = ctx.data.listen(({ details: event }) => {
      if (closed) return;
      if (event.type === 'server.connected') {
        for (const family of families.values()) family.snapshot = { events: [], inFlight: false, retryAt: 0 };
      }
      captureLocalSession();
      let change: SessionEvent;
      switch (event.type) {
        case 'session.inbox.enqueued':
          if (event.data.item.type === 'user') void capturePrompt(event.data.sessionID, event.data.item.payload.text);
          return;
        case 'session.created':
          change = { type: 'created', sessionID: event.data.sessionID, parentID: event.data.parentID };
          deleted.delete(change.sessionID);
          break;
        case 'session.execution.started':
          change = { type: 'started', sessionID: event.data.sessionID };
          break;
        case 'session.execution.succeeded':
        case 'session.execution.failed':
        case 'session.execution.interrupted':
          change = { type: 'finished', sessionID: event.data.sessionID };
          break;
        case 'session.deleted':
          change = { type: 'deleted', sessionID: event.data.sessionID };
          break;
        case 'permission.asked':
          change = { type: 'asked', sessionID: event.data.sessionID, requestID: `permission:${event.data.id}` };
          break;
        case 'permission.replied':
          change = { type: 'replied', sessionID: event.data.sessionID, requestID: `permission:${event.data.requestID}` };
          break;
        case 'form.created':
          change = { type: 'asked', sessionID: event.data.form.sessionID, requestID: `form:${event.data.form.id}` };
          break;
        case 'form.replied':
        case 'form.cancelled':
          change = { type: 'replied', sessionID: event.data.sessionID, requestID: `form:${event.data.id}` };
          break;
        default:
          return;
      }
      for (const family of families.values()) family.snapshot?.events.push(change);
      const family = owner(change.sessionID, change.parentID);
      if (family) observe(family, change);
      if (change.type === 'deleted') {
        deleted.add(change.sessionID);
        families.delete(change.sessionID);
        if (visibleRoot === change.sessionID) visibleRoot = undefined;
      }
      publish();
    });

    captureLocalSession();
    // Snapshot cache commits and route changes do not emit server events.
    const timer = setInterval(captureLocalSession, 500);
    return () => {
      closed = true;
      clearInterval(timer);
      stop();
      families.clear();
    };
  },
});
