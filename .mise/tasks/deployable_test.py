"""deployable's split between images to build and declarations to apply alone, against this
repository's own apps: `python3 .mise/tasks/deployable_test.py`."""

import importlib.machinery
import importlib.util
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
loader = importlib.machinery.SourceFileLoader("deployable", str(HERE / "deployable"))
spec = importlib.util.spec_from_loader("deployable", loader)
deployable = importlib.util.module_from_spec(spec)
loader.exec_module(deployable)

DECLARED = deployable.apps()


def split(*files, revisions=None):
	"""The images `files` need rebuilt, and the declarations they leave to apply alone."""
	rebuilt = deployable.needed(list(files), DECLARED, revisions)
	return rebuilt, deployable.redeclared(list(files), DECLARED, rebuilt, revisions)


class Revision:
	"""A revision holding `texts`, path to text, and the working copy's text of any other path."""

	def __init__(self, texts):
		self.texts = texts

	def read(self, path):
		if path in self.texts:
			return self.texts[path]
		file = deployable.ROOT / path
		return file.read_text() if file.exists() else None


def edited(path, change):
	"""`path` changed by `change`, a function of its text, as the files and revisions to compare."""
	before = (deployable.ROOT / path).read_text()
	return {"revisions": (Revision({path: before}), Revision({path: change(before)}))}


class Declarations(unittest.TestCase):
	def test_a_changed_declaration_alone_is_applied_without_an_image(self):
		self.assertEqual(split("apps/observe/meter/service.toml"), ([], ["meter"]))

	def test_a_declaration_beside_anything_else_of_its_app_is_built(self):
		files = ("apps/network/caddy/service.toml", "apps/network/caddy/Dockerfile")
		self.assertEqual(split(*files), (["caddy"], []))

	def test_a_declaration_a_binary_includes_rebuilds_that_binary(self):
		# host includes its own for itself; keeper includes host's for recreating it.
		self.assertEqual(split("apps/deploy/host/service.toml"), (["host", "keeper"], []))

	def test_only_the_apps_own_declaration_is_one(self):
		self.assertTrue(deployable.declaration("apps/observe/meter/service.toml"))
		self.assertFalse(deployable.declaration("apps/observe/meter/src/service.toml"))
		self.assertFalse(deployable.declaration("apps/observe/meter/Dockerfile"))
		self.assertFalse(deployable.declaration("libs/deploy/fixtures/geo.toml"))

	def test_a_test_include_carries_nothing_into_a_binary(self):
		carried = deployable.embedded(deployable.rust_graph()[0])
		# keeper's tests read its own declaration; its binary does not.
		self.assertNotIn("keeper", carried.get("apps/deploy/keeper/service.toml", set()))
		self.assertIn("host", carried.get("apps/deploy/host/service.toml", set()))


METER = "apps/observe/meter/src/main.rs"


class Fingerprints(unittest.TestCase):
	def test_a_comment_in_rust_is_no_change(self):
		change = edited(METER, lambda text: "// A note.\n" + text.replace("\n", "\n/* and */\n", 3))
		self.assertEqual(split(METER, **change), ([], []))

	def test_a_reformat_is_no_change(self):
		change = edited(METER, lambda text: text.replace("\t", "    ").replace(" {\n", "\n{\n"))
		self.assertEqual(split(METER, **change), ([], []))

	def test_a_changed_token_is_a_rebuild(self):
		change = edited(METER, lambda text: text + "\nfn another() {}\n")
		self.assertEqual(split(METER, **change), (["meter"], []))
		# Given no revisions, a change is a change.
		self.assertEqual(split(METER), (["meter"], []))

	def test_prose_and_tests_build_nothing(self):
		for path in ("apps/observe/meter/README.md", "apps/deploy/host/tests/reach.rs", "libs/deploy/LICENSE"):
			self.assertEqual(split(path), ([], []), path)

	def test_a_comment_in_a_declaration_is_no_change_and_a_value_is_one(self):
		declaration = "apps/observe/meter/service.toml"
		self.assertEqual(split(declaration, **edited(declaration, lambda text: "# Why.\n" + text)), ([], []))
		changed = edited(declaration, lambda text: text.replace('name = "meter"', 'name = "meter"\nhealth_timeout = 9'))
		self.assertEqual(split(declaration, **changed), ([], ["meter"]))

	def test_a_declaration_a_binary_includes_is_its_bytes(self):
		declaration = "apps/deploy/host/service.toml"
		change = edited(declaration, lambda text: "# Why.\n" + text)
		self.assertEqual(split(declaration, **change), (["host", "keeper"], []))

	def test_a_comment_in_a_dockerfile_is_no_change(self):
		dockerfile = "apps/observe/meter/Dockerfile"
		self.assertEqual(split(dockerfile, **edited(dockerfile, lambda text: text + "\n# Done.\n")), ([], []))
		changed = edited(dockerfile, lambda text: text.replace("FROM ", "FROM --platform=$BUILDPLATFORM ", 1))
		self.assertEqual(split(dockerfile, **changed), (["meter"], []))

	def test_the_build_is_every_image_unless_only_its_comments_moved(self):
		ignore = ".dockerignore"
		self.assertEqual(split(ignore, **edited(ignore, lambda text: text + "# More.\n")), ([], []))
		self.assertEqual(split(ignore, **edited(ignore, lambda text: text + "**/cache\n"))[0], sorted(DECLARED))

	def test_a_binarys_directory_holds_nothing_else_its_image_is_built_from(self):
		# host's scripts, its compose file and its package are run beside it, never built into it.
		for path in ("apps/deploy/host/scripts/host.ts", "apps/deploy/host/package.json"):
			self.assertEqual(split(path), ([], []), path)
		self.assertEqual(deployable.cargo_built(DECLARED, deployable.rust_graph()[0]), {"host", "keeper", "meter"})
		# caddy is built from no crate, so anything in its directory is its image's.
		self.assertEqual(split("apps/network/caddy/extra.conf"), (["caddy"], []))

	def test_what_a_dockerfile_copies_from_the_context(self):
		dockerfile = "FROM a AS build\nCOPY Cargo.toml Cargo.lock ./\nCOPY apps apps\nADD --checksum=x \\\n\thttps://e.test/a.tgz /a\nCOPY --from=build /x /x\n"
		self.assertEqual(deployable.copied(dockerfile), {"Cargo.toml", "Cargo.lock", "apps"})
		self.assertEqual(deployable.copied(dockerfile + "COPY units /units\n"), {"Cargo.toml", "Cargo.lock", "apps", "units"})
		self.assertIsNone(deployable.copied('COPY ["a", "b"]\n'))


if __name__ == "__main__":
	unittest.main()
