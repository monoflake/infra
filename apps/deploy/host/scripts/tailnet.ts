/**
 * host's API on one node, reached over the tailnet by SSH: a loopback port here forwarded to host's
 * address on its own network, since host publishes none and Caddy passes on only the notice and
 * the console's reads. The same way in as `mise run node`'s verbs, .mise/tasks/host_api.py. See
 * the workspace's spec/architecture/layers.md, "Four places, and which way they lean".
 */
import { spawn, spawnSync } from 'node:child_process';
import { connect, createServer } from 'node:net';

/** host's port, in apps/deploy/host/src/config.rs, on host's network, `network_of("host")`. */
const PORT = 11011;
const NETWORK = 'app-host';
/** How long the forwarded port gets to start answering. */
const FORWARD_MS = 15_000;
const SSH = ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10'];

function fail(message: string): never {
	console.error(message);
	process.exit(1);
}

/** host's address on its own network, as Docker on the node gives it. */
function addressOf(node: string): string {
	const template = `{{with index .NetworkSettings.Networks "${NETWORK}"}}{{.IPAddress}}{{end}}`;
	const asked = spawnSync('ssh', [...SSH, `root@${node}`, `docker inspect -f '${template}' host`], {
		encoding: 'utf8',
	});
	if (asked.status !== 0) fail(`root@${node} did not answer over SSH, or runs no host`);
	return asked.stdout.trim() || fail(`host on ${node} has no address on ${NETWORK}`);
}

/** A loopback port nobody holds, for ssh's end of the forward. */
function freePort(): Promise<number> {
	return new Promise((resolve, reject) => {
		const probe = createServer();
		probe.once('error', reject);
		probe.listen(0, '127.0.0.1', () => {
			const address = probe.address();
			probe.close(() =>
				typeof address === 'object' && address ? resolve(address.port) : reject(),
			);
		});
	});
}

function answers(port: number): Promise<boolean> {
	return new Promise((resolve) => {
		const socket = connect(port, '127.0.0.1');
		socket.once('connect', () => resolve(!socket.destroy()));
		socket.once('error', () => resolve(false));
	});
}

/**
 * Run `work` with `http://127.0.0.1:<port>` standing for host on `node`, and close the forward
 * after it, however it ended. `work` returns rather than exits, or the forward outlives it.
 */
export async function throughTailnet<T>(node: string, work: (base: string) => T): Promise<T> {
	const port = await freePort();
	const target = `127.0.0.1:${port}`;
	const forward = [...SSH, '-N', '-o', 'ExitOnForwardFailure=yes'];
	const session = spawn(
		'ssh',
		[...forward, '-L', `${target}:${addressOf(node)}:${PORT}`, `root@${node}`],
		{ stdio: ['ignore', 'ignore', 'inherit'] },
	);
	try {
		const deadline = Date.now() + FORWARD_MS;
		// oxlint-disable-next-line no-await-in-loop -- a poll, each asked after the last wait
		while (!(await answers(port))) {
			if (session.exitCode !== null) throw new Error(`forwarding to ${node} failed`);
			if (Date.now() > deadline) {
				throw new Error(`forwarding to ${node} did not answer in ${FORWARD_MS}ms`);
			}
			// oxlint-disable-next-line no-await-in-loop -- the wait between two asks
			await new Promise((resolve) => setTimeout(resolve, 200));
		}
		return work(`http://${target}`);
	} finally {
		session.kill();
	}
}
