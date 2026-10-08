import { createServer } from 'node:net';
import { afterEach, expect, test } from 'vitest';

import { answers } from './tailnet.ts';

const servers: ReturnType<typeof createServer>[] = [];

afterEach(async () => {
	await Promise.all(
		servers.splice(0).map(
			(server) =>
				new Promise<void>((resolve, reject) =>
					server.close((error) => (error ? reject(error) : resolve())),
				),
		),
	);
});

test('reports a listening port as answering', async () => {
	const server = createServer();
	servers.push(server);
	await new Promise<void>((resolve, reject) => {
		server.once('error', reject);
		server.listen(0, '127.0.0.1', resolve);
	});
	const address = server.address();
	if (!address || typeof address === 'string') throw new Error('server has no TCP address');

	await expect(answers(address.port)).resolves.toBe(true);
});
