"""host's API on one node, as `node apps|events|restart|rollback|remove|deploy` asks it: over the
tailnet by SSH, through a port forwarded to host's address on its own network, since host
publishes none.
It needs nothing above the node -- not Caddy, not the tunnel, not the edge -- so it still answers
when they are what broke. See the workspace's spec/architecture/layers.md, "Four places, and which
way they lean". A library rather than a task, since it is not executable.
"""

import json
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# host's and keeper's ports, in apps/deploy/host/src/config.rs and apps/deploy/keeper/src/main.rs,
# on the network host answers on, `network_of("host")` in libs/deploy.
HOST = ("host", 11011)
KEEPER = ("keeper", 11010)
NETWORK = "app-host"
# A restart or a rollback answers once the app passed its health check, which can take minutes.
SECONDS = 600
# How long the forwarded port gets to start answering.
FORWARD_SECONDS = 15
REPOSITORY = "monoflake/infra"
# This machine's end of the forward: a port only it answers on, never an address of anything.
LOOPBACK = "127.0.0.1"

VERBS = ("apps", "events", "restart", "redeploy", "rollback", "remove", "deploy")

USAGE = """usage:
  node apps <name> [--json]
  node events <name> [app] [--limit N] [--json]
  node restart <name> <app>
  node redeploy <name> <app>
  node rollback <name> <app> [--with-data]
  node remove <name> <app> [--drop-data]
  node deploy <name> --run RUN_ID [--repository OWNER/NAME] [--app APP]
each also takes --dry-run, to say what it would ask and ask nothing
--app deploys that one app of the run, and is how an app rolled out by hand is deployed

  mise run node apps tyo
  mise run node events rdu geo --limit 20
  mise run node restart nrt geo
  mise run node redeploy tyo deployer
  mise run node rollback buf geo --with-data
  mise run node remove tyo panel
  mise run node deploy hnd --run 12345678 --repository monoflake/platform
  mise run node deploy rdu --run 12345678 --repository monoflake/platform --app database"""


def fail(message):
	sys.exit(f"node: {message}")


def token(name, dry):
	"""HOST_TOKEN_<NAME>, from the environment mise decrypts secrets.json into, or from sops by a
	pipe when the task is run outside mise. Never printed and never on a command line."""
	key = f"HOST_TOKEN_{name.upper()}"
	if os.environ.get(key):
		return os.environ[key]
	read = subprocess.run(
		["sops", "decrypt", "--extract", json.dumps([key]), "secrets.json"],
		cwd=ROOT,
		text=True,
		stdout=subprocess.PIPE,
		stderr=subprocess.DEVNULL,
	)
	if read.returncode == 0 and read.stdout.strip():
		return read.stdout.strip()
	if dry:
		return None
	fail(f"{key} is not in secrets.json")


def ssh_output(name, command):
	result = subprocess.run(
		["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", f"root@{name}", command],
		text=True,
		stdout=subprocess.PIPE,
	)
	return result.stdout.strip() if result.returncode == 0 else None


def address_of(name, container):
	"""The container's address on host's network, as Docker on the node gives it."""
	on = f'{{{{with index .NetworkSettings.Networks "{NETWORK}"}}}}'
	template = f"{on}{{{{.IPAddress}}}}{{{{end}}}}"
	address = ssh_output(name, f"docker inspect -f '{template}' {container}")
	if address is None:
		fail(f"root@{name} did not answer over SSH, or has no container {container}")
	if not address:
		fail(f"{container} on {name} has no address on {NETWORK}")
	return address


def free_port():
	with socket.socket() as probe:
		probe.bind((LOOPBACK, 0))
		return probe.getsockname()[1]


class Forward:
	"""Each container's port on the node, answered on a port of this machine's loopback while the
	block runs. The SSH session is the only way in, and it ends with the block."""

	def __init__(self, name, targets):
		self.name = name
		self.targets = targets
		self.ports = {}
		self.session = None

	def __enter__(self):
		spec = []
		for container, port in self.targets:
			local = free_port()
			self.ports[container] = local
			spec += ["-L", f"{LOOPBACK}:{local}:{address_of(self.name, container)}:{port}"]
		self.session = subprocess.Popen(
			["ssh", "-N", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
				"-o", "ExitOnForwardFailure=yes", *spec, f"root@{self.name}"],
		)
		deadline = time.monotonic() + FORWARD_SECONDS
		for local in self.ports.values():
			while True:
				if self.session.poll() is not None:
					fail(f"forwarding to {self.name} failed")
				try:
					socket.create_connection((LOOPBACK, local), timeout=1).close()
					break
				except OSError:
					if time.monotonic() > deadline:
						fail(f"forwarding to {self.name} did not answer in {FORWARD_SECONDS}s")
					time.sleep(0.2)
		return self

	def __exit__(self, *_):
		if self.session and self.session.poll() is None:
			self.session.terminate()
			self.session.wait()


def ask(port, method, path, secret, body=None):
	"""host's envelope for one request. The token reaches curl on stdin as a header line."""
	headers = f"authorization: Bearer {secret}\n" if secret else ""
	arguments = ["curl", "--silent", "--show-error", "--max-time", str(SECONDS), "--header", "@-",
		"-X", method]
	if body is not None:
		headers += "content-type: application/json\n"
		arguments += ["--data", json.dumps(body)]
	sent = subprocess.run(
		[*arguments, f"http://{LOOPBACK}:{port}{path}"],
		input=headers,
		text=True,
		stdout=subprocess.PIPE,
	)
	if sent.returncode != 0:
		fail(f"{method} {path} reached nothing")
	try:
		return json.loads(sent.stdout)
	except json.JSONDecodeError:
		fail(f"{method} {path} answered nothing readable: {sent.stdout[:200]}")


def data_of(envelope, what):
	if envelope.get("status") != "success":
		fail(f"{what}: {envelope.get('code')}: {envelope.get('message')}")
	return envelope.get("data")


def table(columns, rows):
	if not rows:
		return "(empty)"
	widths = [max(len(column), *(len(row[i]) for row in rows)) for i, column in enumerate(columns)]

	def line(cells):
		return "  ".join(cell.ljust(width) for cell, width in zip(cells, widths)).rstrip()

	return "\n".join([line(columns), *map(line, rows)])


def text(value):
	return "" if value is None else value if isinstance(value, str) else json.dumps(value)


def show_apps(apps):
	rows = [
		[
			app["manifest"]["name"],
			text(app.get("running")).lower(),
			text(app.get("held")).lower(),
			text(app.get("image"))[:19],
			text(app.get("deployed_at")),
		]
		for app in apps
	]
	return table(["name", "running", "held", "image", "deployed_at"], rows)


def show_events(events):
	def source(event):
		kind = event.get("source", {})
		return f"{kind.get('kind', '')} {kind.get('run', '')}".strip()

	rows = [
		[
			text(event["id"]),
			event["app"],
			text(event["action"]),
			source(event),
			text(event["outcome"]),
			text(event.get("stage")),
			event["started_at"],
			text(event.get("detail")).splitlines()[0][:60] if event.get("detail") else "",
		]
		for event in events
	]
	columns = ["id", "app", "action", "source", "outcome", "stage", "started_at", "detail"]
	return table(columns, rows)


def option(arguments, flag):
	if flag not in arguments:
		return None
	at = arguments.index(flag)
	if at + 1 >= len(arguments):
		fail(f"{flag} takes a value\n{USAGE}")
	return arguments[at + 1]


def plan(verb, arguments):
	"""What a verb asks: each request as (container, method, path, body), and how to show it."""
	valued = {option(arguments, flag) for flag in ("--limit", "--run", "--repository", "--app")}
	words = [a for a in arguments if not a.startswith("-") and a not in valued]
	app = words[0] if words else None
	if verb == "apps":
		return [(HOST, "GET", "/api/apps", None)], show_apps
	if verb == "events":
		limit = option(arguments, "--limit")
		if limit is not None and not limit.isdigit():
			fail(f"--limit takes a number\n{USAGE}")
		query = f"?limit={limit}" if limit else ""
		path = f"/api/apps/{app}/history{query}" if app else f"/api/events{query}"
		return [(HOST, "GET", path, None)], show_events
	if verb in ("restart", "redeploy", "rollback", "remove") and app is None:
		fail(f"{verb} takes an app\n{USAGE}")
	if verb == "restart":
		return [(HOST, "POST", f"/api/apps/{app}/restart", None)], None
	# The current version started again, which is what reads its environment anew. See
	# spec/architecture/host.md, "An app's environment is two files".
	if verb == "redeploy":
		return [(HOST, "POST", f"/api/apps/{app}/redeploy", None)], None
	if verb == "rollback":
		body = {"with_data": "--with-data" in arguments}
		return [(HOST, "POST", f"/api/apps/{app}/rollback", body)], None
	if verb == "remove":
		# Its directory and snapshots stay unless asked; see apps/deploy/host/src/rollout/remove.rs.
		dropped = "?data=drop" if "--drop-data" in arguments else ""
		return [(HOST, "DELETE", f"/api/apps/{app}{dropped}", None)], None
	run = option(arguments, "--run")
	if run is None or not run.isdigit():
		fail(f"deploy takes --run and a run's number\n{USAGE}")
	# Both receivers the hook tells, so a run that built host reaches keeper as well. See
	# spec/architecture/host.md, "A run that built host is keeper's first, and host's only after".
	notice = {"run": int(run), "repository": option(arguments, "--repository") or REPOSITORY}
	only = option(arguments, "--app")
	if only is None:
		return [(HOST, "POST", "/notice", notice), (KEEPER, "POST", "/notice", notice)], None
	# One app of the run, host's alone: keeper deploys host and nothing else. See
	# spec/architecture/host.md, "An app chooses how it is rolled out, and keeping nothing earns a
	# gapless one".
	if only == "host":
		fail(f"host is keeper's to deploy; leave out --app\n{USAGE}")
	return [(HOST, "POST", "/notice", {**notice, "app": only})], None


def main(verb, rest, nodes):
	dry = "--dry-run" in rest or "-n" in rest
	if not rest or rest[0] not in nodes:
		fail(f"{verb} takes one of {', '.join(nodes)} first\n{USAGE}")
	name = rest[0]
	requests, show = plan(verb, rest[1:])
	needs = any(path.startswith("/api/") for _, _, path, _ in requests)
	secret = token(name, dry) if needs else None
	if dry:
		print(f"node: would ssh root@{name}, forward to {NETWORK} on it, and ask:")
		for (container, port), method, path, body in requests:
			sent = f" {json.dumps(body)}" if body is not None else ""
			print(f"  {method} http://{container}:{port}{path}{sent}")
		if needs:
			found = "found" if secret else "missing"
			print(f"  with HOST_TOKEN_{name.upper()} from secrets.json ({found}) on curl's stdin")
		return
	targets = sorted({target for target, *_ in requests})
	with Forward(name, targets) as forward:
		for (container, _), method, path, body in requests:
			# A notice is open on both receivers, so the token goes to host's API alone.
			carried = secret if path.startswith("/api/") else None
			envelope = ask(forward.ports[container], method, path, carried, body)
			data = data_of(envelope, f"{container} {method} {path}")
			if show is None:
				print(f"{container}: {json.dumps(data)}")
			else:
				print(json.dumps(data, indent=2) if "--json" in rest else show(data))
