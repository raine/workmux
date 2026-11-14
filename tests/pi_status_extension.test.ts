import { describe, expect, mock, test } from 'bun:test';
import { EventEmitter } from 'node:events';

type Spawned = { args: string[]; cwd: string; prompt: string };
let spawned: Spawned[] = [];
let spawnResult: number | 'error' | 'throw' = 0;
let onSpawn: ((spawn: Spawned) => void) | undefined;

mock.module('node:child_process', () => ({
  spawn(_command: string, args: string[], options: { cwd: string }) {
    if (spawnResult === 'throw') throw new Error('spawn failed');
    const child = new EventEmitter() as EventEmitter & { stdin: EventEmitter & { end(data: string): void } };
    const stdin = new EventEmitter() as EventEmitter & { end(data: string): void };
    stdin.end = (data: string) => {
      const record = { args, cwd: options.cwd, prompt: JSON.parse(data).prompt };
      spawned.push(record);
      onSpawn?.(record);
      queueMicrotask(() => {
        if (spawnResult === 'error') child.emit('error', new Error('ENOENT'));
        else child.emit('close', spawnResult);
      });
    };
    child.stdin = stdin;
    return child;
  },
}));

const { default: workmuxStatusExtension } = await import('../resources/pi/extensions/workmux-status');

type Handler = (event: unknown, context: unknown) => Promise<void> | void;
type Listener = (data: unknown) => Promise<void> | void;
type AssistantMessage = {
  role: 'assistant';
  stopReason: string;
  errorMessage?: string;
};

const stoppedMessage = (): AssistantMessage => ({
  role: 'assistant',
  stopReason: 'stop',
});

const abortedMessages: AssistantMessage[] = [
  { role: 'assistant', stopReason: 'aborted' },
  { role: 'assistant', stopReason: 'error', errorMessage: 'The operation was aborted.' },
];

async function createHarness(
  initialMessage = stoppedMessage(),
  options: {
    snapshot?: number;
    idle?: boolean;
    exec?: (args: string[]) => Promise<number>;
  } = {},
) {
  const handlers = new Map<string, Handler>();
  const listeners = new Map<string, Set<Listener>>();
  const pending = new Set<Promise<void>>();
  const calls: string[][] = [];
  const statuses: string[] = [];
  let branch = [{ type: 'message', message: initialMessage }];
  spawned = [];
  spawnResult = 0;
  onSpawn = (record) => {
    if (record.args[0] === 'set-window-status' && spawnResult === 0) statuses.push(record.args[1]);
  };
  let idle = options.idle ?? true;
  const events = {
    on(name: string, listener: Listener) {
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name)!.add(listener);
      return () => { listeners.get(name)!.delete(listener); };
    },
    emit(name: string, data: unknown) {
      // Pi invokes listeners synchronously but does not await their promises.
      for (const listener of listeners.get(name) ?? []) {
        const promise = Promise.resolve(listener(data));
        pending.add(promise);
        void promise.finally(() => pending.delete(promise));
      }
    },
  };
  let stopPublisher: (() => void) | undefined;
  if (options.snapshot !== undefined) {
    stopPublisher = events.on('suba:activity:request', () => {
      events.emit('suba:activity', { activeCount: options.snapshot });
    });
  }
  const pi = {
    events,
    exec: async (_command: string, args: string[]) => {
      calls.push(args);
      const code = await options.exec?.(args) ?? 0;
      if (args[0] === 'set-window-status' && code === 0) statuses.push(args[1]);
      return { stdout: '', stderr: '', code, killed: false };
    },
    on: (name: string, handler: Handler) => handlers.set(name, handler),
  };
  workmuxStatusExtension(pi as never);

  const harness = {
    calls,
    statuses,
    events,
    listeners,
    stopPublisher() { stopPublisher?.(); },
    async flush() {
      while (pending.size) await Promise.all([...pending]);
    },
    async activity(activeCount: number) {
      events.emit('suba:activity', { activeCount });
      await this.flush();
    },
    setMessage(message: AssistantMessage) {
      branch = [...branch, { type: 'message', message }];
    },
    async input(text: string, source = 'interactive', cwd = '/project') {
      await handlers.get('input')?.({ type: 'input', text, source }, { cwd });
    },
    async emit(name: string, isIdle?: boolean, aborted?: boolean) {
      if (name === 'agent_start') idle = false;
      if (name === 'agent_settled') idle = true;
      if (isIdle !== undefined) idle = isIdle;
      await handlers.get(name)?.({ aborted }, {
        isIdle: () => idle,
        sessionManager: { getBranch: () => branch },
      });
    },
  };
  await harness.emit('session_start');
  return harness;
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => { resolve = done; });
  return { promise, resolve };
}

describe('pi workmux status extension', () => {
  test('registers without changing status when no publisher is present', async () => {
    const harness = await createHarness();
    expect(harness.calls).toEqual([['register-agent']]);
    await harness.emit('session_shutdown');
    expect(harness.statuses).toEqual([]);
  });

  test('reports done only after the full agent run settles', async () => {
    const harness = await createHarness();

    await harness.emit('agent_start');
    await harness.emit('agent_end');
    expect(harness.statuses).toEqual(['working']);

    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test.each(abortedMessages)('clears an aborted turn without a publisher', async (message) => {
    const harness = await createHarness(message);

    await harness.emit('agent_start');
    await harness.emit('agent_settled');

    expect(harness.statuses).toEqual(['working', 'clear']);
  });

  test('uses the settled abort signal even without an aborted assistant message', async () => {
    const harness = await createHarness();
    await harness.emit('agent_start');
    await harness.emit('agent_settled', true, true);
    expect(harness.statuses).toEqual(['working', 'clear']);
  });

  test('an explicit non-aborted settle overrides historical aborted messages', async () => {
    const harness = await createHarness(abortedMessages[0]);
    await harness.emit('agent_start');
    await harness.emit('agent_settled', true, false);
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('child snapshots cannot restore a completion checkmark after an abort', async () => {
    const harness = await createHarness();
    await harness.emit('agent_start');
    await harness.emit('agent_settled', true, true);
    await harness.activity(0);
    await harness.activity(1);
    await harness.activity(0);
    await harness.emit('session_shutdown');
    expect(harness.statuses).toEqual(['working', 'clear', 'working', 'clear']);
  });

  test('reports done after the continuation completes', async () => {
    const harness = await createHarness(abortedMessages[1]);

    await harness.emit('agent_start');
    await harness.emit('agent_settled');
    harness.setMessage(stoppedMessage());
    await harness.emit('agent_start');
    await harness.emit('agent_settled');

    expect(harness.statuses).toEqual(['working', 'clear', 'working', 'done']);
  });

  test('ordinary errors still report done without a publisher', async () => {
    const harness = await createHarness({
      role: 'assistant', stopReason: 'error', errorMessage: 'Provider unavailable',
    });
    await harness.emit('agent_start');
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test.each([0, 2])('requests a restored snapshot after registration (%i children)', async (snapshot) => {
    const harness = await createHarness(stoppedMessage(), { snapshot });
    expect(harness.calls).toEqual([
      ['register-agent'],
      ['set-window-status', snapshot > 0 ? 'working' : 'done'],
    ]);
  });

  test('accepts a publisher starting after the consumer', async () => {
    const harness = await createHarness();
    await harness.activity(2);
    expect(harness.statuses).toEqual(['working']);
  });

  test('restoring children does not hide an already active parent', async () => {
    const harness = await createHarness(stoppedMessage(), { snapshot: 0, idle: false });
    expect(harness.statuses).toEqual(['working']);
  });

  test('keeps working until every concurrent child becomes inactive', async () => {
    const harness = await createHarness();
    await harness.emit('agent_start');
    await harness.activity(2);
    await harness.emit('agent_end');
    await harness.emit('agent_settled');
    await harness.activity(1);
    await harness.activity(1);
    expect(harness.statuses).toEqual(['working']);
    await harness.activity(0);
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('keeps working while the parent runs after the last child stops', async () => {
    const harness = await createHarness();
    await harness.activity(1);
    await harness.emit('agent_start');
    await harness.activity(0);
    await harness.emit('agent_end');
    expect(harness.statuses).toEqual(['working']);
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('does not settle a parent run started by another extension', async () => {
    const harness = await createHarness();
    await harness.emit('agent_start');
    await harness.activity(0);
    await harness.emit('agent_settled', false);
    expect(harness.statuses).toEqual(['working']);
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('waiting children stop contributing and resume without a parent turn', async () => {
    const harness = await createHarness();
    await harness.activity(1);
    await harness.activity(0);
    await harness.activity(1);
    await harness.activity(0);
    expect(harness.statuses).toEqual(['working', 'done', 'working', 'done']);
  });

  test.each(abortedMessages)('keeps active children working after parent abort, then clears', async (message) => {
    const harness = await createHarness(message);
    await harness.emit('agent_start');
    await harness.activity(1);
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working']);
    await harness.activity(0);
    expect(harness.statuses).toEqual(['working', 'clear']);
    harness.setMessage(stoppedMessage());
    await harness.emit('agent_start');
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'clear', 'working', 'done']);
  });

  test('clears a settled aborted parent with a zero-child snapshot', async () => {
    const harness = await createHarness(abortedMessages[0]);
    await harness.emit('agent_start');
    await harness.activity(0);
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'clear']);
  });

  test('ignores malformed snapshots without enabling aggregate behavior', async () => {
    const harness = await createHarness(abortedMessages[0]);
    await harness.emit('agent_start');
    for (const data of [null, undefined, 2, {}, { activeCount: '1' },
      { activeCount: -1 }, { activeCount: 0.5 }, { activeCount: NaN },
      { activeCount: Infinity }, { activeCount: Number.MAX_SAFE_INTEGER + 1 }]) {
      harness.events.emit('suba:activity', data);
    }
    await harness.flush();
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'clear']);
  });

  test('serializes overlapping child and parent status commands', async () => {
    const gate = deferred();
    const started = deferred();
    const harness = await createHarness(stoppedMessage(), {
      async exec(args) {
        if (args[1] === 'working') {
          started.resolve();
          await gate.promise;
        }
        return 0;
      },
    });
    harness.events.emit('suba:activity', { activeCount: 1 });
    await started.promise;
    harness.events.emit('suba:activity', { activeCount: 0 });
    const parentStart = harness.emit('agent_start');
    expect(harness.calls).toEqual([['register-agent'], ['set-window-status', 'working']]);
    gate.resolve();
    await parentStart;
    await harness.flush();
    expect(harness.statuses).toEqual(['working', 'done', 'working']);
  });

  test.each(['reject', 'nonzero'])('a %s command failure does not poison later writes', async (failure) => {
    let attempts = 0;
    const harness = await createHarness(stoppedMessage(), {
      async exec(args) {
        if (args[0] === 'set-window-status' && attempts++ === 0) {
          if (failure === 'reject') throw new Error('workmux unavailable');
          return 1;
        }
        return 0;
      },
    });
    await harness.activity(1);
    await harness.activity(1);
    await harness.activity(0);
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('registration failure does not prevent status updates', async () => {
    const harness = await createHarness(stoppedMessage(), {
      async exec(args) {
        if (args[0] === 'register-agent') throw new Error('not in tmux');
        return 0;
      },
    });
    await harness.activity(1);
    await harness.activity(0);
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('shutdown drains pending writes, clears activity, and removes its listener', async () => {
    const gate = deferred();
    const started = deferred();
    const harness = await createHarness(stoppedMessage(), {
      async exec(args) {
        if (args[1] === 'working') {
          started.resolve();
          await gate.promise;
        }
        return 0;
      },
    });
    harness.events.emit('suba:activity', { activeCount: 1 });
    await started.promise;
    let stopped = false;
    const shutdown = harness.emit('session_shutdown').then(() => { stopped = true; });
    await harness.emit('agent_start');
    harness.events.emit('suba:activity', { activeCount: 2 });
    expect(stopped).toBe(false);
    expect(harness.listeners.get('suba:activity')!.size).toBe(0);
    gate.resolve();
    await shutdown;
    await harness.flush();
    await harness.emit('session_shutdown');
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('session replacement discards child counts and ordinary abort behavior returns', async () => {
    const harness = await createHarness(abortedMessages[0], { snapshot: 2 });
    await harness.emit('session_shutdown');
    harness.stopPublisher();
    await harness.emit('session_start');
    await harness.emit('agent_start');
    await harness.emit('agent_settled');
    expect(harness.statuses).toEqual(['working', 'clear', 'working', 'clear']);
    expect(harness.listeners.get('suba:activity')!.size).toBe(1);
  });

  test('reload requests a fresh snapshot and does not accumulate listeners', async () => {
    const harness = await createHarness(stoppedMessage(), { snapshot: 2 });
    await harness.emit('session_shutdown');
    await harness.emit('session_start');
    expect(harness.statuses).toEqual(['working', 'done', 'working']);
    expect(harness.listeners.get('suba:activity')!.size).toBe(1);
    await harness.activity(0);
    expect(harness.statuses.at(-1)).toBe('done');
  });

  test('sends a submitted prompt with the agent start report', async () => {
    const harness = await createHarness();

    await harness.input('fix the bug', 'interactive', '/repo/feature');
    expect(spawned).toEqual([]);
    await harness.emit('agent_start');
    await harness.emit('agent_settled');

    expect(spawned).toEqual([
      { args: ['set-window-status', 'working'], cwd: '/repo/feature', prompt: 'fix the bug' },
    ]);
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('sends a steering prompt at once while the agent works', async () => {
    const harness = await createHarness(stoppedMessage(), { snapshot: 0 });

    await harness.emit('agent_start');
    await harness.input('also update docs');

    expect(spawned.map((record) => record.prompt)).toEqual(['also update docs']);
    expect(harness.statuses).toEqual(['done', 'working', 'working']);
  });

  test('does not report working for a prompt while nothing is active', async () => {
    const gate = deferred();
    const harness = await createHarness(stoppedMessage(), {
      async exec(args) {
        if (args[1] === 'done') await gate.promise;
        return 0;
      },
    });
    await harness.activity(1);
    harness.events.emit('suba:activity', { activeCount: 0 });
    await harness.input('next task');
    gate.resolve();
    await harness.flush();

    expect(harness.statuses).toEqual(['working', 'done']);
    expect(spawned).toEqual([]);
  });

  test('ignores extension-sourced and blank input', async () => {
    const harness = await createHarness();

    await harness.input('injected', 'extension');
    await harness.input('  ');
    await harness.emit('agent_start');

    expect(spawned).toEqual([]);
    expect(harness.statuses).toEqual(['working']);
  });

  test.each(['error', 'throw', 1] as const)('a %s prompt write does not poison later writes', async (failure) => {
    const harness = await createHarness(stoppedMessage(), { snapshot: 0 });

    spawnResult = failure;
    await harness.input('fix the bug');
    await harness.emit('agent_start');
    spawnResult = 0;
    await harness.emit('agent_settled');
    await harness.activity(1);

    expect(harness.statuses).toEqual(['done', 'done', 'working']);
  });
});
