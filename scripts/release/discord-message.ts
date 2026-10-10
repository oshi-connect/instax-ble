const DISCORD_EMBED_DESCRIPTION_LIMIT = 4096;
const TRUNCATION_SUFFIX = '…';

export type DiscordReleaseEmbedInput = {
	product: string;
	tag: string;
	url: string;
	body: string;
};

export type DiscordReleaseMessage = {
	embeds: Array<{
		title: string;
		url: string;
		description: string;
	}>;
};

export const truncateForDiscordEmbed = (body: string): string => {
	if (body.length <= DISCORD_EMBED_DESCRIPTION_LIMIT) {
		return body;
	}

	const maxContentLength = DISCORD_EMBED_DESCRIPTION_LIMIT - TRUNCATION_SUFFIX.length;
	return `${body.slice(0, maxContentLength)}${TRUNCATION_SUFFIX}`;
};

export const buildDiscordReleaseMessage = (
	input: DiscordReleaseEmbedInput,
): DiscordReleaseMessage => ({
	embeds: [
		{
			title: `${input.product} ${input.tag}`,
			url: input.url,
			description: truncateForDiscordEmbed(input.body),
		},
	],
});
