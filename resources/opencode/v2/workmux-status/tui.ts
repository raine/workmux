import { spawn } from 'node:child_process';
import { Plugin } from '@opencode/plugin/tui';
import { StatusTracker, type WindowStatus } from './status';

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

function workmux(...args: string[]): Promise<boolean> {
  return new Promise((resolve) => {
    // Workmux reads stdin for hook metadata. An open pipe makes it wait forever.
    const child = spawn('workmux', args, { stdio: 'ignore', timeout: 3000 });
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
    let queue = workmux('register-agent').then(() => {});

    const tracker = new StatusTracker((status) => {
      desired = status;
      queue = queue.then(async () => {
        if (closed || desired === reported || !desired) return;
        const next = desired;
        if (await workmux('set-window-status', next)) reported = next;
      });
    });

    // Server events and the tabs list are shared between clients. Only this
    // TUI's current route establishes ownership of a session family.
    type Lease = {
      members: Set<string>;
      hydrating: boolean;
      hydrated: boolean;
      hydration: ReturnType<StatusTracker['beginHydration']>;
    };
    const roots = new Map<string, Lease>();
    const owned = new Map<string, string>();
    const parents = new Map<string, string>();
    const deleted = new Set<string>();
    let visibleRoot: string | undefined;

    function isOwned(sessionID: string): boolean {
      if (deleted.has(sessionID)) return false;
      const existing = owned.get(sessionID);
      if (existing && roots.has(existing)) return true;
      const root = ctx.data.session.root(sessionID);
      const parent = parents.get(sessionID) ?? ctx.data.session.get(sessionID)?.parentID;
      const owner = !deleted.has(root) && roots.has(root)
        ? root
        : parent && isOwned(parent) ? owned.get(parent) : undefined;
      if (!owner) return false;
      owned.set(sessionID, owner);
      roots.get(owner)?.members.add(sessionID);
      return true;
    }

    function releaseIfIdle(root: string) {
      const lease = roots.get(root);
      if (!lease || visibleRoot === root || lease.hydrating) return;
      if (!lease.hydrated && !deleted.has(root)) return;
      if ([...lease.members].some((sessionID) => tracker.isActive(sessionID))) return;
      roots.delete(root);
      lease.hydration.finish();
      for (const sessionID of lease.members) {
        if (owned.get(sessionID) === root) owned.delete(sessionID);
        tracker.forget(sessionID);
        parents.delete(sessionID);
      }
    }

    async function hydrate(root: string, lease: Lease) {
      lease.hydrating = true;
      let hydrated = false;
      try {
        // The family cache may be empty until the root's metadata is loaded.
        await ctx.data.session.sync(root);
        if (closed || roots.get(root) !== lease || deleted.has(root)) return;
        const family = new Set([root, ...ctx.data.session.family(root)]);
        // Claim the complete known family before any child awaits. Its events
        // may arrive before the child's own root/parent metadata is cached.
        for (const sessionID of family) {
          if (deleted.has(sessionID)) continue;
          owned.set(sessionID, root);
          lease.members.add(sessionID);
        }
        for (const sessionID of family) {
          if (closed || roots.get(root) !== lease) return;
          if (deleted.has(sessionID)) continue;
          if (sessionID !== root) await ctx.data.session.sync(sessionID);
          await Promise.all([
            ctx.data.session.permission.sync(sessionID),
            ctx.data.session.form.sync(sessionID),
          ]);
          if (closed || roots.get(root) !== lease || deleted.has(root) || deleted.has(sessionID)) continue;
          const pending = [
            ...(ctx.data.session.permission.list(sessionID) ?? []).map((request) => `permission:${request.id}`),
            ...(ctx.data.session.form.list(sessionID) ?? []).map((form) => `form:${form.id}`),
          ];
          if (ctx.data.session.status(sessionID) === 'running' || pending.length) {
            lease.hydration.seed(sessionID, pending);
          }
        }
        hydrated = true;
      } catch (error) {
        // Keep the completion barrier until reconciliation can load the family,
        // even if its tab closed while a background child was being discovered.
        console.warn('[workmux.status] could not hydrate session status', error);
      } finally {
        if (hydrated || closed || deleted.has(root)) lease.hydration.finish();
        if (roots.get(root) !== lease) return;
        lease.hydrating = false;
        lease.hydrated = hydrated;
        releaseIfIdle(root);
      }
    }

    function claim(sessionID: string) {
      if (deleted.has(sessionID)) return;
      const root = ctx.data.session.root(sessionID);
      if (deleted.has(root)) return;
      let lease = roots.get(root);
      if (!lease) {
        lease = { members: new Set(), hydrating: false, hydrated: false, hydration: tracker.beginHydration() };
        roots.set(root, lease);
      }
      owned.set(sessionID, root);
      lease.members.add(sessionID);
      if (!lease.hydrating && !lease.hydrated) {
        void hydrate(root, lease);
      }
    }

    function captureLocalSessions() {
      const route = ctx.ui.router.current();
      const sessionID = route.type === 'session' ? route.sessionID : undefined;
      const root = sessionID && !deleted.has(sessionID) ? ctx.data.session.root(sessionID) : undefined;
      visibleRoot = root && !deleted.has(root) ? root : undefined;
      if (sessionID && visibleRoot) claim(sessionID);
      for (const [ownedRoot, lease] of roots) {
        if (!lease.hydrating && !lease.hydrated && !deleted.has(ownedRoot)) void hydrate(ownedRoot, lease);
        releaseIfIdle(ownedRoot);
      }
    }

    const stop = ctx.data.listen(({ details: event }) => {
      if (closed) return;
      captureLocalSessions();

      if (event.type === 'session.created') {
        deleted.delete(event.data.sessionID);
        if (event.data.parentID) {
          parents.set(event.data.sessionID, event.data.parentID);
          if (isOwned(event.data.parentID)) isOwned(event.data.sessionID);
        }
        return;
      }

      const sessionID = event.type === 'form.created'
        ? event.data.form.sessionID
        : 'sessionID' in event.data && typeof event.data.sessionID === 'string'
          ? event.data.sessionID
          : undefined;
      if (!sessionID || !isOwned(sessionID)) return;
      const owner = owned.get(sessionID);

      switch (event.type) {
        case 'session.execution.started':
          tracker.start(sessionID);
          break;
        case 'session.status':
          if (event.data.status.type === 'busy' || event.data.status.type === 'retry') tracker.busy(sessionID);
          if (event.data.status.type === 'idle') tracker.finish(sessionID);
          break;
        case 'permission.asked':
          tracker.wait(sessionID, `permission:${event.data.id}`);
          break;
        case 'permission.replied':
          tracker.resume(sessionID, `permission:${event.data.requestID}`);
          break;
        case 'form.created':
          tracker.wait(sessionID, `form:${event.data.form.id}`);
          break;
        case 'form.replied':
        case 'form.cancelled':
          tracker.resume(sessionID, `form:${event.data.id}`);
          break;
        case 'session.execution.succeeded':
        case 'session.execution.failed':
        case 'session.execution.interrupted':
        case 'session.idle':
          tracker.finish(sessionID);
          break;
        case 'session.deleted':
          tracker.forget(sessionID);
          owned.delete(sessionID);
          parents.delete(sessionID);
          if (owner) roots.get(owner)?.members.delete(sessionID);
          if (visibleRoot === sessionID) visibleRoot = undefined;
          deleted.add(sessionID);
          roots.get(sessionID)?.hydration.finish();
          break;
      }
      if (owner) releaseIfIdle(owner);
    });

    captureLocalSessions();
    // A newly opened session can become visible after its first server event.
    // Reconcile client-local navigation so its already-running turn is picked up.
    const timer = setInterval(captureLocalSessions, 500);
    return () => {
      closed = true;
      clearInterval(timer);
      stop();
      for (const lease of roots.values()) lease.hydration.finish();
    };
  },
});
