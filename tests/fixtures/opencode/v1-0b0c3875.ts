import type { Plugin } from '@opencode-ai/plugin';

export const WorkmuxStatusPlugin: Plugin = async ({ $ }) => {
  return {
    event: async ({ event }) => {
      switch (event.type) {
        case 'session.status':
          if (event.properties.status.type === 'busy') {
            await $`workmux set-window-status working`.quiet();
          }
          break;
        case 'permission.updated':
          await $`workmux set-window-status waiting`.quiet();
          break;
        case 'permission.replied':
          await $`workmux set-window-status working`.quiet();
          break;
        case 'session.idle':
          await $`workmux set-window-status done`.quiet();
          break;
      }
    },
  };
};
