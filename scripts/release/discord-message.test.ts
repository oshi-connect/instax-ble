import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
	buildDiscordReleaseMessage,
	truncateForDiscordEmbed,
} from './discord-message.ts';

test('title is the product and the tag', () => {
	const message = buildDiscordReleaseMessage({
		product: 'instax-ble',
		tag: 'v2026.10.10',
		url: 'https://github.com/oshi-connect/instax-ble/releases/tag/v2026.10.10',
		body: 'First release.',
	});

	assert.equal(message.embeds.length, 1);
	assert.equal(message.embeds[0]?.title, 'instax-ble v2026.10.10');
	assert.equal(
		message.embeds[0]?.url,
		'https://github.com/oshi-connect/instax-ble/releases/tag/v2026.10.10',
	);
	assert.equal(message.embeds[0]?.description, 'First release.');
});

test('truncates the description at 4096 characters with an ellipsis', () => {
	const body = 'a'.repeat(4097);
	const description = truncateForDiscordEmbed(body);

	assert.equal(description.length, 4096);
	assert.equal(description, `${'a'.repeat(4095)}…`);

	const message = buildDiscordReleaseMessage({
		product: 'instax-ble',
		tag: 'v2026.10.10.2',
		url: 'https://example.com/release',
		body,
	});
	assert.equal(message.embeds[0]?.description, description);
});
