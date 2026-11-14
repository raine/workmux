import { describe, expect, mock, test } from 'bun:test';
import { EventEmitter } from 'node:events';

type Spawned = { args: string[]; cwd: string; prompt: string };
let spawned: Spawned[] = [];
let releaseSpawn: Promise<void> = Promise.resolve();

mock.module('node:child_process', () => ({
  spawn(_command: string, args: string[], options: { cwd: string }) {
    const child = new EventEmitter() as EventEmitter & { stdin: EventEmitter & { end(data: string): void } };
    const stdin = new EventEmitter() as EventEmitter & { end(data: string): void };
    stdin.end = (data: string) => {
      spawned.push({ args, cwd: options.cwd, prompt: JSON.parse(data).prompt });
      void releaseSpawn.then(() => child.emit('close', 0));
    };
    child.stdin = stdin;
    return child;
  },
}));

const { default: workmuxStatusExtension } = await import('../resources/omp/extensions/workmux-status');

type Handler = (event: unknown, context: unknown) => Promise<void> | void;

function createHarness() {
  const handlers = new Map<string, Handler>();
  const calls: string[][] = [];
  const statuses: string[] = [];
  spawned = [];
  releaseSpawn = Promise.resolve();
  const pi = {
    exec: async (_command: string, args: string[]) => {
      calls.push(args);
      if (args[0] === 'set-window-status') statuses.push(args[1]);
      return { stdout: '', stderr: '', code: 0, killed: false };
    },
    on: (name: string, handler: Handler) => handlers.set(name, handler),
  };
  workmuxStatusExtension(pi as never);

  return {
    calls,
    statuses,
    async emit(name: string, event: unknown = {}) {
      await handlers.get(name)?.(event, {});
    },
    async input(text: string, source = 'interactive') {
      await handlers.get('input')?.({ type: 'input', text, source }, { cwd: '/repo/feature' });
    },
  };
}

describe('omp workmux status extension', () => {
  test('does not report waiting between an assistant tool call and execution', async () => {
    const harness = createHarness();

    await harness.emit('session_start');
    await harness.emit('agent_start');
    await harness.emit('message_end', {
      message: {
        role: 'assistant',
        content: [{ type: 'toolCall', name: 'bash' }],
      },
    });
    await harness.emit('tool_call', { toolName: 'bash' });
    await harness.emit('tool_execution_start');
    await harness.emit('agent_end');

    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('reports waiting only for the ask tool', async () => {
    const harness = createHarness();

    await harness.emit('session_start');
    await harness.emit('agent_start');
    await harness.emit('message_end', {
      message: { role: 'assistant', content: [{ type: 'text', text: 'Question' }] },
    });
    await harness.emit('tool_call', { toolName: 'ask' });

    expect(harness.statuses).toEqual(['working', 'waiting']);
    await harness.emit('agent_end');
    expect(harness.statuses).toEqual(['working', 'waiting', 'done']);
  });

  test('sends a submitted prompt with the agent start report', async () => {
    const harness = createHarness();

    await harness.emit('session_start');
    await harness.input('fix the bug');
    await harness.emit('agent_start');
    await harness.emit('tool_execution_start');
    await harness.emit('agent_end');

    expect(spawned).toEqual([
      { args: ['set-window-status', 'working'], cwd: '/repo/feature', prompt: 'fix the bug' },
    ]);
    expect(harness.statuses).toEqual(['done']);
  });

  test('sends a steering prompt at once while the agent works', async () => {
    const harness = createHarness();

    await harness.emit('session_start');
    await harness.emit('agent_start');
    await harness.input('also update docs');

    expect(harness.statuses).toEqual(['working']);
    expect(spawned.map((record) => record.prompt)).toEqual(['also update docs']);
  });

  test('ignores shell, python, extension and blank input', async () => {
    const harness = createHarness();

    await harness.emit('session_start');
    await harness.emit('agent_start');
    for (const text of ['!git status', '  $ print(1)', '$$ x', '   ']) {
      await harness.input(text);
    }
    await harness.input('injected', 'extension');
    await harness.emit('agent_end');
    await harness.emit('agent_start');

    expect(spawned).toEqual([]);
  });

  test('a built-in command that starts no turn is not sent as a prompt', async () => {
    const harness = createHarness();

    await harness.emit('session_start');
    await harness.emit('agent_start');
    await harness.input('/model');
    expect(spawned).toEqual([]);
    await harness.emit('agent_end');
    await harness.emit('agent_start');

    expect(spawned).toEqual([]);
    expect(harness.statuses).toEqual(['working', 'done', 'working']);
  });

  test('reports done only after a pending prompt write completes', async () => {
    const harness = createHarness();
    let release!: () => void;
    releaseSpawn = new Promise((resolve) => { release = resolve; });

    await harness.emit('session_start');
    await harness.input('fix the bug');
    const start = harness.emit('agent_start');
    const end = harness.emit('agent_end');
    await Promise.resolve();
    expect(harness.statuses).toEqual([]);

    release();
    await Promise.all([start, end]);
    expect(harness.statuses).toEqual(['done']);
    expect(spawned).toHaveLength(1);
  });
});
