import { describe, expect, test } from 'bun:test';

import { WorkmuxStatusPlugin } from '../resources/opencode/plugins/workmux-status';

async function createHarness({
  failRegistration = false,
  sessions = {} as Record<string, { parentID?: string } | Error>,
} = {}) {
  const statuses: string[] = [];
  const commands: string[] = [];
  const prompts: Array<{ status: string; prompt: string }> = [];
  const lookups: string[] = [];
  const shell = (strings: TemplateStringsArray, ...values: unknown[]) => {
    const status = values[0] as string | undefined;
    const stdin = values.find((value) => value instanceof Response) as Response | undefined;
    const command = strings.reduce(
      (result, part, index) =>
        result + part + (index < values.length ? (values[index] === stdin ? '<stdin>' : values[index]) : ''),
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
        if (stdin && status !== undefined) {
          prompts.push({ status, prompt: JSON.parse(await stdin.text()).prompt });
        }
      },
    };
  };
  const client = {
    session: {
      get: async ({ path }: { path: { id: string } }) => {
        lookups.push(path.id);
        const session = sessions[path.id];
        if (session instanceof Error) throw session;
        return { data: session === undefined ? undefined : { id: path.id, ...session } };
      },
    },
  };
  const hooks = await WorkmuxStatusPlugin({ $: shell, client } as never);

  return {
    commands,
    statuses,
    prompts,
    lookups,
    emit: async (event: unknown) => {
      await hooks.event?.({ event } as never);
    },
    message: async (sessionID: string, parts: unknown[]) => {
      await hooks['chat.message']?.({ sessionID } as never, { message: {}, parts } as never);
    },
  };
}

const textPart = (text: string, synthetic?: boolean) => ({ type: 'text', text, synthetic });

const sessionCreated = (id: string, parentID?: string) => ({
  type: 'session.created',
  properties: { info: { id, parentID } },
});

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

  test('sends a submitted prompt with the next working report', async () => {
    const harness = await createHarness();

    await harness.emit(sessionCreated('parent'));
    await harness.message('parent', [textPart('fix the'), textPart('bug')]);
    expect(harness.statuses).toEqual([]);

    await harness.emit(userMessage('parent'));
    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit(sessionStatus('parent', 'idle'));

    expect(harness.statuses).toEqual(['working', 'done']);
    expect(harness.prompts).toEqual([{ status: 'working', prompt: 'fix the\nbug' }]);
  });

  test('sends a prompt queued while working without a status change', async () => {
    const harness = await createHarness();

    await harness.emit(sessionCreated('parent'));
    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.message('parent', [textPart('also update docs')]);

    expect(harness.statuses).toEqual(['working', 'working']);
    expect(harness.prompts).toEqual([{ status: 'working', prompt: 'also update docs' }]);
  });

  test('holds a prompt while waiting until work resumes', async () => {
    const harness = await createHarness();

    await harness.emit(sessionCreated('parent'));
    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.emit({ type: 'permission.asked', properties: { sessionID: 'parent' } });
    await harness.message('parent', [textPart('yes, go ahead')]);
    expect(harness.statuses).toEqual(['working', 'waiting']);
    expect(harness.prompts).toEqual([]);

    await harness.emit({ type: 'permission.replied', properties: { sessionID: 'parent' } });
    expect(harness.prompts).toEqual([{ status: 'working', prompt: 'yes, go ahead' }]);
  });

  test('ignores prompts of child sessions and synthetic parts', async () => {
    const harness = await createHarness();

    await harness.emit(sessionCreated('parent'));
    await harness.emit(sessionCreated('child', 'parent'));
    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.message('child', [textPart('delegated task')]);
    await harness.message('parent', [textPart('file contents', true)]);
    await harness.emit(sessionStatus('child', 'busy'));

    expect(harness.statuses).toEqual(['working']);
    expect(harness.prompts).toEqual([]);
    expect(harness.lookups).toEqual([]);
  });

  test('classifies unknown sessions through the client', async () => {
    const harness = await createHarness({
      sessions: { parent: {}, child: { parentID: 'parent' }, failing: new Error('offline') },
    });

    await harness.emit(sessionStatus('parent', 'busy'));
    await harness.message('child', [textPart('delegated task')]);
    await harness.message('failing', [textPart('unknown')]);
    await harness.message('missing', [textPart('unknown')]);
    expect(harness.prompts).toEqual([]);

    await harness.message('parent', [textPart('top level')]);
    await harness.message('child', [textPart('cached')]);
    expect(harness.lookups).toEqual(['child', 'failing', 'missing', 'parent']);
    expect(harness.prompts).toEqual([{ status: 'working', prompt: 'top level' }]);
  });
});
