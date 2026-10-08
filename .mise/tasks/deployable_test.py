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


def split(*files):
	"""The images `files` need rebuilt, and the declarations they leave to apply alone."""
	rebuilt = deployable.needed(list(files), DECLARED)
	return rebuilt, deployable.redeclared(list(files), DECLARED, rebuilt)


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


if __name__ == "__main__":
	unittest.main()
