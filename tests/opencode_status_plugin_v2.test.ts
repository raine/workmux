import { afterEach, beforeEach, expect, mock, test } from 'bun:test';
import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

mock.module('@opencode/plugin/tui', () => ({ Plugin: { define: (definition: unknown) => definition } }));

const { default: plugin } = await import('../resources/opencode/v2/workmux-status/tui');
const originalPath = process.env.PATH;
const originalLog = process.env.WORKMUX_TEST_LOG;
const muxVariables = ['TMUX', 'TMUX_PANE', 'WEZTERM_PANE', 'ZELLIJ', 'ZELLIJ_PANE_ID', 'ZELLIJ_SESSION_NAME', 'KITTY_WINDOW_ID', 'WORKMUX_BACKEND'] as const;
const originalMux = Object.fromEntries(muxVariables.map((name) => [name, process.env[name]]));
let testDir: string;

beforeEach(async () => {
  for (const name of muxVariables) delete process.env[name];
  testDir = await mkdtemp(join(tmpdir(), 'workmux-status-test-'));
  const binary = join(testDir, 'workmux');
  // The real workmux reads stdin to check for hook input. This stub does too:
  // leaving execFile's stdin pipe open prevents it from registering at all.
  await writeFile(binary, '#!/bin/sh\n/bin/cat >/dev/null\ncase "$WORKMUX_BACKEND" in\n  wezterm) pane="$WEZTERM_PANE";;\n  zellij) pane="$ZELLIJ_PANE_ID";;\n  kitty) pane="$KITTY_WINDOW_ID";;\n  *) pane="${TMUX_PANE:-${WEZTERM_PANE:-${ZELLIJ_PANE_ID:-${KITTY_WINDOW_ID:-}}}}";;\nesac\nprintf "%s|%s\\n" "$pane" "$*" >> "$WORKMUX_TEST_LOG"\n');
  await chmod(binary, 0o755);
  process.env.PATH = `${testDir}:${originalPath}`;
  process.env.WORKMUX_TEST_LOG = join(testDir, 'calls');
});

async function calls() {
  const log = await readFile(process.env.WORKMUX_TEST_LOG!, 'utf8').catch(() => '');
  return log.trim() ? log.trim().split('\n') : [];
}

async function waitForCalls(count: number) {
  for (let i = 0; i < 80; i++) {
    if ((await calls()).length >= count) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  expect(await calls()).toHaveLength(count);
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => { resolve = done; });
  return { promise, resolve };
}

async function hydrationHarness(options: {
  family?: string[];
  root?: (id: string) => string;
  sync?: (id: string) => Promise<void>;
  permissionSync?: () => Promise<void>;
  pending?: string[];
  pendingSession?: string;
  forms?: string[];
} = {}) {
  process.env.TMUX = 'test-tmux';
  process.env.TMUX_PANE = '%123';
  let handler: (input: any) => void = () => {};
  let open = true;
  const cleanup = await plugin.setup({
    data: {
      listen: (callback: typeof handler) => { handler = callback; return () => {}; },
      session: {
        root: options.root ?? (() => 'ours'),
        family: () => options.family ?? ['ours'],
        get: () => undefined,
        status: () => 'running',
        sync: options.sync ?? (async () => {}),
        permission: {
          sync: options.permissionSync ?? (async () => {}),
          list: (sessionID: string) => options.pendingSession && options.pendingSession !== sessionID
            ? [] : (options.pending ?? []).map((id) => ({ id })),
        },
        form: { sync: async () => {}, list: () => (options.forms ?? []).map((id) => ({ id })) },
      },
    },
    ui: { router: { current: () => open ? { type: 'session', sessionID: 'ours' } : { type: 'home' } } },
  } as any);
  return {
    close: () => { open = false; },
    cleanup: async () => { if (typeof cleanup === 'function') await cleanup(); },
    emit: (type: string, data: object) => handler({ details: { type, data } }),
  };
}

afterEach(async () => {
  for (const name of muxVariables) {
    const original = originalMux[name];
    if (original === undefined) delete process.env[name];
    else process.env[name] = original;
  }
  process.env.PATH = originalPath;
  if (originalLog === undefined) delete process.env.WORKMUX_TEST_LOG;
  else process.env.WORKMUX_TEST_LOG = originalLog;
  await rm(testDir, { recursive: true });
});

test('hydration reconciles pending requests after a busy event', async () => {
  const permissions = deferred();
  const harness = await hydrationHarness({
    permissionSync: () => permissions.promise,
    pending: ['allow-edit'],
  });
  try {
    await waitForCalls(1);
    harness.emit('session.status', { sessionID: 'ours', status: { type: 'busy' } });
    await waitForCalls(2);
    permissions.resolve();
    await waitForCalls(3);
    expect(await calls()).toEqual([
      '%123|register-agent',
      '%123|set-window-status working',
      '%123|set-window-status waiting',
    ]);
  } finally {
    permissions.resolve();
    await harness.cleanup();
  }
});

test.each(['permission.replied', 'form.replied', 'form.cancelled'])('hydration does not restore a request after %s', async (type) => {
  const permissions = deferred();
  const harness = await hydrationHarness({
    permissionSync: () => permissions.promise,
    pending: type === 'permission.replied' ? ['request'] : [],
    forms: type !== 'permission.replied' ? ['request'] : [],
  });
  try {
    await waitForCalls(1);
    harness.emit('session.status', { sessionID: 'ours', status: { type: 'busy' } });
    await waitForCalls(2);
    harness.emit(type, { sessionID: 'ours', requestID: 'request', id: 'request' });
    permissions.resolve();
    await Bun.sleep(50);
    expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working']);
  } finally {
    permissions.resolve();
    await harness.cleanup();
  }
});

test('hydration never reports done while a running child is still loading', async () => {
  const child = deferred();
  const harness = await hydrationHarness({
    family: ['ours', 'child'],
    sync: async (id) => { if (id === 'child') await child.promise; },
  });
  try {
    await waitForCalls(2);
    harness.close();
    harness.emit('session.execution.succeeded', { sessionID: 'ours' });
    await Bun.sleep(50);
    expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working']);
    child.resolve();
    await Bun.sleep(50);
    expect(await calls()).toHaveLength(2);
    harness.emit('session.execution.succeeded', { sessionID: 'child' });
    await waitForCalls(3);
    expect((await calls()).at(-1)).toBe('%123|set-window-status done');
    harness.emit('session.execution.started', { sessionID: 'child' });
    await Bun.sleep(50);
    expect(await calls()).toHaveLength(3);
  } finally {
    child.resolve();
    await harness.cleanup();
  }
});

test.each(['permission.replied', 'session.execution.succeeded', 'session.deleted'])('hydration captures child %s before its parent metadata loads', async (type) => {
  const child = deferred();
  const harness = await hydrationHarness({
    family: ['ours', 'child'],
    root: (id) => id, // The family is known, but the child's root/parent metadata is not.
    sync: async (id) => { if (id === 'child') await child.promise; },
    pending: ['request'],
    pendingSession: 'child',
  });
  try {
    await waitForCalls(2);
    harness.emit('session.execution.succeeded', { sessionID: 'ours' });
    harness.emit(type, { sessionID: 'child', requestID: 'request' });
    child.resolve();
    await Bun.sleep(50);
    if (type === 'permission.replied') {
      expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working']);
      harness.emit('session.execution.succeeded', { sessionID: 'child' });
    }
    await waitForCalls(3);
    expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working', '%123|set-window-status done']);
  } finally {
    child.resolve();
    await harness.cleanup();
  }
});

test('hydration retries a failed child sync after the tab closes without premature completion', async () => {
  const firstAttempt = deferred();
  const retry = deferred();
  let attempts = 0;
  const harness = await hydrationHarness({
    family: ['ours', 'child'],
    sync: async (id) => {
      if (id !== 'child') return;
      if (++attempts === 1) {
        await firstAttempt.promise;
        throw new Error('temporary sync failure');
      }
      retry.resolve();
    },
  });
  try {
    await waitForCalls(2);
    harness.close();
    harness.emit('session.execution.succeeded', { sessionID: 'ours' });
    firstAttempt.resolve();
    await Bun.sleep(50);
    expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working']);
    await Promise.race([retry.promise, Bun.sleep(1200)]);
    expect(attempts).toBe(2);
    await Bun.sleep(50);
    expect(await calls()).toHaveLength(2);
    harness.emit('session.execution.succeeded', { sessionID: 'child' });
    await waitForCalls(3);
    expect((await calls()).at(-1)).toBe('%123|set-window-status done');
  } finally {
    firstAttempt.resolve();
    await harness.cleanup();
  }
});

test.each(['session.execution.succeeded', 'session.execution.failed', 'session.execution.interrupted', 'session.deleted'])('hydration cannot restore a session after %s', async (type) => {
  const permissions = deferred();
  const harness = await hydrationHarness({ permissionSync: () => permissions.promise, pending: ['request'] });
  try {
    await waitForCalls(1);
    harness.emit('session.status', { sessionID: 'ours', status: { type: 'busy' } });
    await waitForCalls(2);
    harness.emit(type, { sessionID: 'ours' });
    permissions.resolve();
    await waitForCalls(3);
    await Bun.sleep(50);
    expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working', '%123|set-window-status done']);
  } finally {
    permissions.resolve();
    await harness.cleanup();
  }
});

test('releases closed tabs after background work finishes', async () => {
  process.env.TMUX = 'test-tmux';
  process.env.TMUX_PANE = '%123';
  let handler: (input: any) => void = () => {};
  let tabOpen = true;
  let stopped = false;
  const parents = new Map([['child', 'ours']]);
  const ctx = {
    data: {
      listen(callback: typeof handler) {
        handler = callback;
        return () => { stopped = true; };
      },
      session: {
        root: (id: string) => parents.get(id) ?? id,
        family: (id: string) => id === 'ours' ? ['ours', 'child'] : [id],
        get: (id: string) => parents.has(id) ? { parentID: parents.get(id) } : undefined,
        status: () => 'idle',
        sync: async () => {},
        permission: { sync: async () => {}, list: () => [] },
        form: { sync: async () => {}, list: () => [] },
      },
    },
    ui: {
      router: { current: () => tabOpen ? { type: 'session', sessionID: 'ours' } : { type: 'home' } },
      tabs: { list: () => tabOpen ? [{ sessionID: 'ours' }] : [] },
    },
  };

  const cleanup = await plugin.setup(ctx as any);
  await waitForCalls(1);
  expect(await calls()).toEqual(['%123|register-agent']);
  const emit = (type: string, data: object) => handler({ details: { type, data } });

  emit('session.execution.started', { sessionID: 'other' });
  emit('session.execution.started', { sessionID: 'ours' });
  await waitForCalls(2);
  tabOpen = false;
  emit('session.created', { sessionID: 'child', parentID: 'ours' });
  emit('session.execution.started', { sessionID: 'child' });
  emit('form.created', { form: { id: 'question', sessionID: 'child' } });
  await waitForCalls(3);
  emit('session.execution.succeeded', { sessionID: 'ours' });
  emit('form.replied', { id: 'question', sessionID: 'child' });
  await waitForCalls(4);
  emit('session.execution.succeeded', { sessionID: 'child' });
  await waitForCalls(5);
  emit('session.deleted', { sessionID: 'child' });
  emit('session.status', { sessionID: 'child', status: { type: 'busy' } });

  expect(await calls()).toEqual([
    '%123|register-agent',
    '%123|set-window-status working',
    '%123|set-window-status waiting',
    '%123|set-window-status working',
    '%123|set-window-status done',
  ]);

  // Closing the tab must not make a later, unrelated turn belong to this pane.
  emit('session.execution.started', { sessionID: 'ours' });
  await Bun.sleep(50);
  expect(await calls()).toHaveLength(5);
  emit('permission.asked', { sessionID: 'ours', id: 'allow-edit' });
  emit('permission.replied', { sessionID: 'ours', requestID: 'allow-edit' });
  emit('session.execution.failed', { sessionID: 'ours' });
  await Bun.sleep(50);
  expect(await calls()).toHaveLength(5);
  // Navigating back to the session explicitly acquires it again.
  tabOpen = true;
  emit('session.execution.started', { sessionID: 'ours' });
  await waitForCalls(6);
  expect((await calls()).at(-1)).toBe('%123|set-window-status working');
  if (typeof cleanup === 'function') await cleanup();
  expect(stopped).toBe(true);
});

test('shared tabs do not claim another pane', async () => {
  process.env.TMUX = 'test-tmux';
  const handlers: Array<(input: any) => void> = [];
  // The tabs and server events are shared, but each TUI has its own route.
  const tabs = [{ sessionID: 'ours' }, { sessionID: 'other' }];
  function client(sessionID: string) {
    return {
      data: {
        listen(callback: (input: any) => void) { handlers.push(callback); return () => {}; },
        session: {
          root: (id: string) => id,
          family: (id: string) => [id],
          get: () => undefined,
          status: () => 'idle',
          sync: async () => {},
          permission: { sync: async () => {}, list: () => [] },
          form: { sync: async () => {}, list: () => [] },
        },
      },
      ui: { router: { current: () => ({ type: 'session', sessionID }) }, tabs: { list: () => tabs } },
    };
  }

  process.env.TMUX_PANE = '%123';
  const cleanupA = await plugin.setup(client('ours') as any);
  process.env.TMUX_PANE = '%456';
  const cleanupB = await plugin.setup(client('other') as any);
  await waitForCalls(2);
  process.env.TMUX_PANE = '%123';
  handlers[0]({ details: { type: 'session.execution.started', data: { sessionID: 'ours' } } });
  await waitForCalls(3);
  process.env.TMUX_PANE = '%456';
  handlers[1]({ details: { type: 'session.execution.started', data: { sessionID: 'ours' } } });
  await Bun.sleep(50);
  process.env.TMUX_PANE = '%123';
  handlers[0]({ details: { type: 'session.execution.succeeded', data: { sessionID: 'ours' } } });
  await waitForCalls(4);
  process.env.TMUX_PANE = '%456';
  handlers[1]({ details: { type: 'session.execution.succeeded', data: { sessionID: 'ours' } } });
  await Bun.sleep(50); // allow any wrongly claimed pane to flush its queued status write
  expect((await calls()).filter((call) => call.includes('set-window-status'))).toEqual([
    '%123|set-window-status working',
    '%123|set-window-status done',
  ]);
  // Switch the test process's pane marker before dispatching to each client;
  // separate TUI processes have separate environments in production.
  if (typeof cleanupA === 'function') await cleanupA();
  if (typeof cleanupB === 'function') await cleanupB();
});

test('hydrates after root sync so a running family member is found', async () => {
  process.env.TMUX = 'test-tmux';
  process.env.TMUX_PANE = '%123';
  let rootSynced = false;
  const ctx = {
    data: {
      listen: () => () => {},
      session: {
        root: (id: string) => id,
        family: () => rootSynced ? ['ours', 'child'] : ['ours'],
        get: () => undefined, // the child's metadata is not in this cache yet
        status: (id: string) => id === 'child' ? 'running' : 'idle',
        sync: async (id: string) => { if (id === 'ours') rootSynced = true; },
        permission: { sync: async () => {}, list: () => [] },
        form: { sync: async () => {}, list: () => [] },
      },
    },
    ui: { router: { current: () => ({ type: 'session', sessionID: 'ours' }) }, tabs: { list: () => [] } },
  };
  const cleanup = await plugin.setup(ctx as any);
  await waitForCalls(2);
  expect(await calls()).toEqual(['%123|register-agent', '%123|set-window-status working']);
  if (typeof cleanup === 'function') await cleanup();
});

test('late hydration does not reclaim a deleted root', async () => {
  process.env.TMUX = 'test-tmux';
  process.env.TMUX_PANE = '%123';
  let resume!: () => void;
  const sync = new Promise<void>((resolve) => { resume = resolve; });
  let handler: (input: any) => void = () => {};
  const ctx = {
    data: {
      listen: (callback: typeof handler) => { handler = callback; return () => {}; },
      session: {
        root: (id: string) => id,
        family: () => ['ours'],
        get: () => undefined,
        status: () => 'running',
        sync: () => sync,
        permission: { sync: async () => {}, list: () => [] },
        form: { sync: async () => {}, list: () => [] },
      },
    },
    ui: { router: { current: () => ({ type: 'session', sessionID: 'ours' }) }, tabs: { list: () => [] } },
  };
  const cleanup = await plugin.setup(ctx as any);
  await waitForCalls(1);
  handler({ details: { type: 'session.deleted', data: { sessionID: 'ours' } } });
  resume();
  await Bun.sleep(50);
  handler({ details: { type: 'session.execution.started', data: { sessionID: 'ours' } } });
  await Bun.sleep(50);
  expect(await calls()).toEqual(['%123|register-agent']);
  if (typeof cleanup === 'function') await cleanup();
});

test('closing a tab during hydration keeps discovered background work until completion', async () => {
  process.env.TMUX = 'test-tmux';
  process.env.TMUX_PANE = '%123';
  let resume!: () => void;
  const rootSync = new Promise<void>((resolve) => { resume = resolve; });
  let loaded = false;
  let tabOpen = true;
  let handler: (input: any) => void = () => {};
  const ctx = {
    data: {
      listen: (callback: typeof handler) => { handler = callback; return () => {}; },
      session: {
        root: (id: string) => id === 'child' ? 'ours' : id,
        family: () => loaded ? ['ours', 'child'] : ['ours'],
        get: (id: string) => id === 'child' ? { parentID: 'ours' } : undefined,
        status: (id: string) => id === 'child' ? 'running' : 'idle',
        sync: async (id: string) => {
          if (id === 'ours') { await rootSync; loaded = true; }
        },
        permission: { sync: async () => {}, list: () => [] },
        form: { sync: async () => {}, list: () => [] },
      },
    },
    ui: { router: { current: () => tabOpen ? { type: 'session', sessionID: 'ours' } : { type: 'home' } }, tabs: { list: () => [] } },
  };
  const cleanup = await plugin.setup(ctx as any);
  await waitForCalls(1);
  tabOpen = false;
  handler({ details: { type: 'session.created', data: { sessionID: 'unrelated' } } });
  resume();
  await waitForCalls(2);
  expect((await calls()).at(-1)).toBe('%123|set-window-status working');
  handler({ details: { type: 'session.execution.succeeded', data: { sessionID: 'child' } } });
  await waitForCalls(3);
  handler({ details: { type: 'session.execution.started', data: { sessionID: 'child' } } });
  await Bun.sleep(50);
  expect(await calls()).toHaveLength(3);
  if (typeof cleanup === 'function') await cleanup();
});

test.each([
  { name: 'WezTerm', vars: { WEZTERM_PANE: '81' }, pane: '81' },
  { name: 'Zellij', vars: { ZELLIJ: '1', ZELLIJ_PANE_ID: '23' }, pane: '23' },
  { name: 'Kitty', vars: { KITTY_WINDOW_ID: '42' }, pane: '42' },
  { name: 'explicit WezTerm backend', vars: { TMUX: 'nested', TMUX_PANE: '%3', WEZTERM_PANE: '81', WORKMUX_BACKEND: 'wezterm' }, pane: '81' },
])('reports status for $name', async ({ vars, pane }) => {
  Object.assign(process.env, vars);
  let handler: (input: any) => void = () => {};
  const ctx = {
    data: {
      listen(callback: typeof handler) { handler = callback; return () => {}; },
      session: {
        root: (id: string) => id,
        family: (id: string) => [id],
        get: () => undefined,
        status: () => 'idle',
        sync: async () => {},
        permission: { sync: async () => {}, list: () => [] },
        form: { sync: async () => {}, list: () => [] },
      },
    },
    ui: {
      router: { current: () => ({ type: 'session', sessionID: 'ours' }) },
      tabs: { list: () => [] },
    },
  };
  const cleanup = await plugin.setup(ctx as any);
  await waitForCalls(1);
  handler({ details: { type: 'session.execution.started', data: { sessionID: 'ours' } } });
  await waitForCalls(2);
  expect(await calls()).toEqual([`${pane}|register-agent`, `${pane}|set-window-status working`]);
  if (typeof cleanup === 'function') await cleanup();
});

test.each([
  { name: 'plain terminal', vars: {} },
  { name: 'tmux without pane ID', vars: { TMUX: 'session' } },
  { name: 'empty tmux marker takes precedence over WezTerm', vars: { TMUX: '', WEZTERM_PANE: '81' } },
  { name: 'Zellij without pane ID', vars: { ZELLIJ: '1' } },
  { name: 'explicit backend without its pane ID', vars: { WEZTERM_PANE: '81', WORKMUX_BACKEND: 'tmux' } },
])('does nothing in $name', async ({ vars }) => {
  Object.assign(process.env, vars);
  await plugin.setup({} as any);
  expect(await calls()).toEqual([]);
});
