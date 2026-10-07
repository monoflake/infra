/**
 * `mise run host deploy <name>`: build an app's image for the machine at home and hand it, with
 * its declaration, to host there over the tailnet by SSH -- or, for host itself, to keeper at its
 * own address, since host never replaces itself. Never the tunnel.
 * `image <name> <path>` only builds the archive, which is how host itself is first carried over.
 * See spec/architecture/host.md, "The machine pulls; nothing pushes into it".
 */
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { INFRA } from '@monoflake/urls';
import { throughTailnet } from './tailnet.ts';

const ROOT = fileURLToPath(new URL('../../../../', import.meta.url));
const USAGE = 'usage: host deploy <name> | host image <name> <path>';
/** The machine at home, by its name in nodes/nodes.toml. */
const NODE = 'rdu';

/** The app's directory, under whichever group of `apps/` holds it. See spec/repository.md. */
function appDirectory(name: string): string {
	const found = readdirSync(join(ROOT, 'apps'))
		.map((group) => join(ROOT, 'apps', group, name))
		.find((dir) => existsSync(dir));
	return found ?? fail(`no app named ${name}`);
}

function fail(message: string): never {
	console.error(message);
	process.exit(1);
}

/** The one build, shared with CI; see .mise/tasks/image. */
function build(name: string, archive: string): void {
	if (!existsSync(join(appDirectory(name), 'Dockerfile'))) fail(`${name} has no Dockerfile`);
	execFileSync(join(ROOT, '.mise/tasks/image'), [name, archive], { cwd: ROOT, stdio: 'inherit' });
}

/**
 * Through curl rather than fetch: macOS asks before a program reaches the local network, and
 * node from mise is refused with EHOSTUNREACH until somebody grants it, while curl, being the
 * system's own, is never asked. The token goes in on stdin so it never shows in `ps`.
 */
function upload(address: string, declaration: string, archive: string, token: string): boolean {
	console.log(`handing it to ${address}`);
	const sent = spawnSync(
		'curl',
		[
			'--silent',
			'--show-error',
			'--header',
			'@-',
			'--form',
			`service=@${declaration}`,
			'--form',
			`image=@${archive}`,
			'--write-out',
			'\n%{http_code}',
			address,
		],
		{ input: `authorization: Bearer ${token}\n`, encoding: 'utf8' },
	);
	const lines = `${sent.stdout}`.trimEnd().split('\n');
	const status = Number(lines.pop());
	console.log(lines.join('\n'));
	if (sent.stderr) console.error(sent.stderr.trimEnd());
	return status >= 200 && status < 300;
}

async function deploy(name: string): Promise<void> {
	const token =
		process.env.HOST_TOKEN_RDU ?? fail('HOST_TOKEN_RDU is not set; it comes from secrets.json');
	const declaration = join(appDirectory(name), 'service.toml');
	if (!existsSync(declaration)) fail(`${name} has no service.toml`);
	const scratch = mkdtempSync(join(tmpdir(), 'host-'));
	try {
		const archive = join(scratch, `${name}.tar`);
		build(name, archive);
		// keeper takes host at its own address; host takes every other app under its API.
		const sent =
			name === 'host'
				? upload(`${INFRA.keeper}/apps/host`, declaration, archive, token)
				: await throughTailnet(NODE, (base) =>
						upload(`${base}/api/apps/${name}`, declaration, archive, token),
					);
		if (!sent) process.exitCode = 1;
	} finally {
		rmSync(scratch, { recursive: true, force: true });
	}
}

const [verb, name, path] = process.argv.slice(2);
if (verb === 'deploy' && name) await deploy(name);
else if (verb === 'image' && name && path) build(name, resolve(path));
else fail(USAGE);
