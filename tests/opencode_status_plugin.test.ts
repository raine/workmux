import { describe, expect, test } from 'bun:test';

import { WorkmuxStatusPlugin } from '../resources/opencode/plugins/workmux-status';

async function createHarness({ failRegistration = false } = {}) {
  const statuses: string[] = [];
  const commands: string[] = [];
  const shell = (strings: TemplateStringsArray, status?: string) => {
    const command = strings.reduce(
      (result, part, index) => result + part + (index < strings.length - 1 ? status : ''),
      '',
    );
    return {
      quiet: async () => {
        commands.push(command);
        if (command === 'workmux register-agent' && failRegistration) {
          throw new Error('registration failed');
        }
        if (status !== undefined) {
          statuses.push(status);
        }
      },
    };
  };
  const hooks = await WorkmuxStatusPlugin({ $: shell } as never);

  return {
    commands,
    statuses,
    emit: async (event: unknown) => {
      await hooks.event?.({ event } as never);
    },
  };
}

const sessionStatus = (sessionID: string, type: 'busy' | 'idle') => ({
  type: 'session.status',
  properties: { sessionID, status: { type } },
});

const userMessage = (sessionID: string) => ({
  type: 'message.updated',
  properties: { sessionID, info: { role: 'user', sessionID } },
});

describe('WorkmuxStatusPlugin', () => {
  test('awaits registration during initialization before status handling', async () => {
    let finishRegistration!: () => void;
    const registration = new Promise<void>((resolve) => {
      finishRegistration = resolve;
    });
    let initialized = false;
    const shell = () => ({ quiet: () => registration });

    const initialization = WorkmuxStatusPlugin({ $: shell } as never).then((hooks) => {
      initialized = true;
      return hooks;
    });
    await Promise.resolve();
    expect(initialized).toBe(false);

    finishRegistration();
    const hooks = await initialization;
    expect(initialized).toBe(true);
    expect(hooks.event).toBeDefined();
  });

  test('registers before reporting status', async () => {
    const harness = await createHarness();
    await harness.emit(sessionStatus('parent', 'busy'));

    expect(harness.commands).toEqual([
      'workmux register-agent',
      'workmux set-window-status working',
    ]);
  });

  test('continues status tracking when registration fails', async () => {
    const harness = await createHarness({ failRegistration: true });
    await harness.emit(sessionStatus('parent', 'busy'));

    expect(harness.commands).toEqual([
      'workmux register-agent',
      'workmux set-window-status working',
    ]);
    expect(harness.statuses).toEqual(['working']);
  });

  test('serializes status writes when event callbacks overlap', async () => {
    const commands: string[] = [];
    const applied: string[] = [];
    const completions: Array<() => void> = [];
    const shell = (strings: TemplateStringsArray, status?: string) => {
      const command = strings.reduce(
        (result, part, index) => result + part + (index < strings.length - 1 ? status : ''),
        '',
      );
      return {
        quiet: () => {
          if (status === undefined) {
            return Promise.resolve();
          }
          commands.push(command);
          return new Promise<void>((resolve) => {
            completions.push(() => {
              applied.push(status);
              resolve();
            });
          });
        },
      };
    };
    const hooks = await WorkmuxStatusPlugin({ $: shell } as never);

    const busy = hooks.event?.({ event: sessionStatus('parent', 'busy') } as never);
    const idle = hooks.event?.({ event: sessionStatus('parent', 'idle') } as never);
    await Promise.resolve();
    expect(commands).toEqual(['workmux set-window-status working']);

    completions.shift()?.();
    await busy;
    await Promise.resolve();
    expect(commands).toEqual([
      'workmux set-window-status working',
      'workmux set-window-status done',
    ]);
    expect(applied).toEqual(['working']);

    completions.shift()?.();
    await idle;
    expect(applied).toEqual(['working', 'done']);
  });

  test('stays working when a child session finishes before its parent', async () => {
    const harness = await createHarness();

    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit(sessionStatus('child', 'busy'));
    await harness.emit(sessionStatus('child', 'idle'));

    expect(harness.statuses).toEqual(['working']);

    await harness.emit(sessionStatus('parent', 'idle'));
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('stays working when a parent session idles before its child', async () => {
    const harness = await createHarness();

    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit(sessionStatus('child', 'busy'));
    await harness.emit(sessionStatus('parent', 'idle'));

    expect(harness.statuses).toEqual(['working']);

    await harness.emit(sessionStatus('child', 'idle'));
    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('forgets an active session when OpenCode deletes it', async () => {
    const harness = await createHarness();

    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit(sessionStatus('child', 'busy'));
    await harness.emit(sessionStatus('parent', 'idle'));
    await harness.emit({
      type: 'session.deleted',
      properties: { info: { id: 'child' } },
    });
    await harness.emit(sessionStatus('child', 'busy'));

    expect(harness.statuses).toEqual(['working', 'done']);
  });

  test('ignores deletion of an untracked session', async () => {
    const harness = await createHarness();

    await harness.emit({
      type: 'session.deleted',
      properties: { info: { id: 'historical' } },
    });

    expect(harness.statuses).toEqual([]);
  });

  test('ignores idle status from an untracked session', async () => {
    const harness = await createHarness();

    await harness.emit(sessionStatus('parent', 'idle'));

    expect(harness.statuses).toEqual([]);
  });

  test('ignores stale busy events until a new user message', async () => {
    const harness = await createHarness();

    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit(sessionStatus('parent', 'idle'));
    await harness.emit(sessionStatus('parent', 'busy'));
    expect(harness.statuses).toEqual(['working', 'done']);

    await harness.emit(userMessage('parent'));
    await harness.emit(sessionStatus('parent', 'busy'));
    expect(harness.statuses).toEqual(['working', 'done', 'working']);
  });

  test('reports waiting while another session is working', async () => {
    const harness = await createHarness();

    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit({
      type: 'question.asked',
      properties: { sessionID: 'child' },
    });
    await harness.emit(sessionStatus('parent', 'idle'));
    expect(harness.statuses).toEqual(['working', 'waiting']);

    await harness.emit({
      type: 'question.replied',
      properties: { sessionID: 'child' },
    });
    expect(harness.statuses).toEqual(['working', 'waiting', 'working']);
  });
});
