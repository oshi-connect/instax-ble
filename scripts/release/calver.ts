const CALVER_PREFIX = 'v';

export const formatCalverBase = (now: Date): string => {
	const year = now.getUTCFullYear();
	const month = String(now.getUTCMonth() + 1).padStart(2, '0');
	const day = String(now.getUTCDate()).padStart(2, '0');
	return `${CALVER_PREFIX}${year}.${month}.${day}`;
};

const calverSuffixPattern = (base: string): RegExp =>
	new RegExp(`^${escapeRegExp(base)}(?:\\.(\\d+))?$`);

const escapeRegExp = (value: string): string => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

export const nextCalverTag = (existingTags: readonly string[], now: Date): string => {
	const base = formatCalverBase(now);
	const pattern = calverSuffixPattern(base);
	let maxSuffix = 0;
	let hasMatch = false;

	for (const tag of existingTags) {
		const match = tag.match(pattern);
		if (!match) {
			continue;
		}

		hasMatch = true;
		const suffix = match[1] ? Number.parseInt(match[1], 10) : 1;
		if (suffix > maxSuffix) {
			maxSuffix = suffix;
		}
	}

	if (!hasMatch) {
		return base;
	}

	return `${base}.${maxSuffix + 1}`;
};
