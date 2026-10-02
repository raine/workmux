import { expect, test } from 'bun:test';
import { StatusTracker, type WindowStatus } from '../resources/opencode/v2/workmux-status/status';

function tracker() {
  const reports: WindowStatus[] = [];
  return { reports, status: new StatusTracker((value) => reports.push(value)) };
}

test('reports working, waiting, resumed and done without duplicate writes', () => {
  const { status, reports } = tracker();
  status.start('parent');
  status.busy('parent');
  status.wait('parent', 'permission:1');
  status.busy('parent');
  status.resume('parent', 'permission:1');
  status.finish('parent');
  status.busy('parent');
  expect(reports).toEqual(['working', 'waiting', 'working', 'done']);
});

test('waits until every permission and form is answered', () => {
  const { status, reports } = tracker();
  status.start('parent');
  status.wait('parent', 'permission:1');
  status.wait('parent', 'form:2');
  status.resume('parent', 'permission:1');
  status.resume('parent', 'form:2');
  expect(reports).toEqual(['working', 'waiting', 'working']);
});

test('one idle child does not finish another running or waiting session', () => {
  const { status, reports } = tracker();
  status.start('parent');
  status.start('child');
  status.wait('child', 'form:1');
  status.finish('parent');
  status.resume('child', 'form:1');
  status.finish('child');
  expect(reports).toEqual(['working', 'waiting', 'working', 'done']);
});

test('a new execution rearms busy after completion', () => {
  const { status, reports } = tracker();
  status.start('parent');
  status.finish('parent');
  status.busy('parent');
  status.start('parent');
  expect(reports).toEqual(['working', 'done', 'working']);
});

test('seeding an already running session includes its pending requests', () => {
  const { status, reports } = tracker();
  status.seed('parent', ['permission:1']);
  status.resume('parent', 'permission:1');
  status.forget('parent');
  expect(reports).toEqual(['waiting', 'working', 'done']);
});

test('active state follows execution and permission lifetimes', () => {
  const { status } = tracker();
  expect(status.isActive('parent')).toBe(false);
  status.start('parent');
  expect(status.isActive('parent')).toBe(true);
  status.wait('parent', 'permission:1');
  expect(status.isActive('parent')).toBe(true);
  status.finish('parent');
  expect(status.isActive('parent')).toBe(false);
});
