import { nextCalverTag } from './calver.ts';
import { buildDiscordReleaseMessage } from './discord-message.ts';

const readStdin = async (): Promise<string> => {
	const chunks: Buffer[] = [];
	for await (const chunk of process.stdin) {
		chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
	}
	return Buffer.concat(chunks).toString('utf8');
};

const readFile = async (path: string): Promise<string> => {
	const { readFile: readFileFs } = await import('node:fs/promises');
	return readFileFs(path, 'utf8');
};

const parseTagList = (raw: string): string[] =>
	raw
		.split('\n')
		.map((line) => line.trim())
		.filter((line) => line.length > 0);

const parseIsoDate = (raw: string | undefined): Date => {
	if (!raw) {
		return new Date();
	}

	const parsed = new Date(raw);
	if (Number.isNaN(parsed.getTime())) {
		throw new Error(`Invalid ISO date: ${raw}`);
	}

	return parsed;
};

const printNextTag = async (args: string[]): Promise<void> => {
	const tags = parseTagList(await readStdin());
	const now = parseIsoDate(args[0]);
	process.stdout.write(`${nextCalverTag(tags, now)}\n`);
};

const printDiscordPayload = async (args: string[]): Promise<void> => {
	const [product, tag, url, bodyFile] = args;
	if (!product || !tag || !url || !bodyFile) {
		throw new Error('Usage: cli.ts discord-payload <product> <tag> <url> <body-file>');
	}

	const body = await readFile(bodyFile);
	const payload = buildDiscordReleaseMessage({ product, tag, url, body });
	process.stdout.write(`${JSON.stringify(payload)}\n`);
};

const main = async (): Promise<void> => {
	const [command, ...args] = process.argv.slice(2);

	switch (command) {
		case 'next-tag':
			await printNextTag(args);
			return;
		case 'discord-payload':
			await printDiscordPayload(args);
			return;
		default:
			throw new Error(`Unknown command: ${command ?? '(missing)'}`);
	}
};

main().catch((error: unknown) => {
	const message = error instanceof Error ? error.message : String(error);
	process.stderr.write(`${message}\n`);
	process.exit(1);
});
