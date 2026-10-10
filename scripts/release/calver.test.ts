import assert from 'node:assert/strict';
import { test } from 'node:test';
import { nextCalverTag } from './calver.ts';

const day = new Date('2026-10-10T15:00:00Z');

test('first tag of the day has no numeric suffix', () => {
	assert.equal(nextCalverTag([], day), 'v2026.10.10');
});

test('same-day repeats use .2 then .3', () => {
	assert.equal(nextCalverTag(['v2026.10.10'], day), 'v2026.10.10.2');
	assert.equal(
		nextCalverTag(['v2026.10.10', 'v2026.10.10.2'], day),
		'v2026.10.10.3',
	);
});

test('tags from other days are ignored', () => {
	assert.equal(
		nextCalverTag(['v2026.10.09', 'v2026.10.09.3', 'v2026.10.11.2'], day),
		'v2026.10.10',
	);
});
