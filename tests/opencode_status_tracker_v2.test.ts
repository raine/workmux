import { expect, test } from 'bun:test';
import { applyEvent, isActive, sessionState, windowStatus, type SessionState } from '../resources/opencode/v2/workmux-status/status';

test('derives working, waiting, resumed, and done from execution and requests', () => {
  const sessions = new Map<string, SessionState>();
  applyEvent(sessions, { type: 'started', sessionID: 'parent' });
  expect(windowStatus(sessions.values())).toBe('working');
  applyEvent(sessions, { type: 'asked', sessionID: 'parent', requestID: 'permission:1' });
  expect(windowStatus(sessions.values())).toBe('waiting');
  applyEvent(sessions, { type: 'replied', sessionID: 'parent', requestID: 'permission:1' });
  expect(windowStatus(sessions.values())).toBe('working');
  applyEvent(sessions, { type: 'finished', sessionID: 'parent' });
  expect(windowStatus(sessions.values())).toBe('done');
});

test('waits until every permission and form is answered', () => {
  const sessions = new Map([['parent', sessionState(true)]]);
  applyEvent(sessions, { type: 'asked', sessionID: 'parent', requestID: 'permission:1' });
  applyEvent(sessions, { type: 'asked', sessionID: 'parent', requestID: 'form:2' });
  applyEvent(sessions, { type: 'replied', sessionID: 'parent', requestID: 'permission:1' });
  expect(windowStatus(sessions.values())).toBe('waiting');
  applyEvent(sessions, { type: 'replied', sessionID: 'parent', requestID: 'form:2' });
  expect(windowStatus(sessions.values())).toBe('working');
});

test('one finished parent does not finish a running or waiting child', () => {
  const sessions = new Map([['parent', sessionState(true)], ['child', sessionState(true, ['form:1'])]]);
  applyEvent(sessions, { type: 'finished', sessionID: 'parent' });
  expect(windowStatus(sessions.values())).toBe('waiting');
  applyEvent(sessions, { type: 'replied', sessionID: 'child', requestID: 'form:1' });
  expect(windowStatus(sessions.values())).toBe('working');
  applyEvent(sessions, { type: 'finished', sessionID: 'child' });
  expect(windowStatus(sessions.values())).toBe('done');
});

test('a new execution starts after completion', () => {
  const sessions = new Map([['parent', sessionState(true)]]);
  applyEvent(sessions, { type: 'finished', sessionID: 'parent' });
  applyEvent(sessions, { type: 'started', sessionID: 'parent' });
  expect(windowStatus(sessions.values())).toBe('working');
});

test('replying to a standalone form does not invent a running execution', () => {
  const sessions = new Map<string, SessionState>();
  applyEvent(sessions, { type: 'asked', sessionID: 'parent', requestID: 'form:1' });
  expect(windowStatus(sessions.values())).toBe('waiting');
  applyEvent(sessions, { type: 'replied', sessionID: 'parent', requestID: 'form:1' });
  expect(windowStatus(sessions.values())).toBe('done');
  expect(isActive(sessions.get('parent')!)).toBe(false);
});

test('completion clears pending requests and deletion forgets the session', () => {
  const sessions = new Map([['parent', sessionState(true, ['permission:1'])]]);
  applyEvent(sessions, { type: 'finished', sessionID: 'parent' });
  expect(isActive(sessions.get('parent')!)).toBe(false);
  expect(sessions.get('parent')!.pending.size).toBe(0);
  applyEvent(sessions, { type: 'deleted', sessionID: 'parent' });
  expect(sessions.size).toBe(0);
  expect(windowStatus(sessions.values())).toBe('done');
});
