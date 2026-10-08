"""The tunnel task's VPC service as it asks the API for one:
`python3 .mise/tasks/tunnel_test.py`."""

import importlib.machinery
import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader("tunnel", str(HERE / "tunnel"))
spec = importlib.util.spec_from_loader("tunnel", loader)
tunnel = importlib.util.module_from_spec(spec)
loader.exec_module(tunnel)

OWN = "0191dce4-9ab4-7fce-b660-8e5dec5172da"
CARRIER = "0191dce4-0000-7fce-b660-8e5dec5172db"


class Body(unittest.TestCase):
	def test_a_node_reaches_caddy_through_its_own_tunnel(self):
		self.assertEqual(
			tunnel.body("tyo", tunnel.target(OWN)),
			{
				"type": "http",
				"name": "tyo",
				"http_port": 80,
				"host": {"hostname": "caddy", "resolver_network": {"tunnel_id": OWN}},
			},
		)

	def test_a_carried_node_is_reached_at_its_tailnet_address_through_the_carriers_tunnel(self):
		wanted = tunnel.body("sha", tunnel.target(OWN, CARRIER, "100.79.131.127"))
		self.assertEqual(wanted["host"], {"ipv4": "100.79.131.127", "network": {"tunnel_id": CARRIER}})
		self.assertEqual((wanted["name"], wanted["type"], wanted["http_port"]), ("sha", "http", 80))


class InPlace(unittest.TestCase):
	def test_a_service_reaching_where_it_should_is_left_as_it_is(self):
		answered = {
			"service_id": "x",
			"type": "http",
			"name": "tyo",
			"http_port": 80,
			"https_port": None,
			"host": {"hostname": "caddy", "resolver_network": {"tunnel_id": OWN, "resolver_ips": None}},
			"created_at": "2026-10-01T00:00:00Z",
		}
		self.assertTrue(tunnel.reaches(answered, tunnel.body("tyo", tunnel.target(OWN))))
		# No port answered is the API's default, 80.
		del answered["http_port"]
		self.assertTrue(tunnel.reaches(answered, tunnel.body("tyo", tunnel.target(OWN))))

	def test_a_service_reaching_elsewhere_is_changed_both_ways(self):
		own = {"http_port": 80, "host": {"hostname": "caddy", "resolver_network": {"tunnel_id": OWN}}}
		carried = tunnel.body("sha", tunnel.target(OWN, CARRIER, "100.79.131.127"))
		self.assertFalse(tunnel.reaches(own, carried))
		# And back, once `tunnel_via` is gone.
		self.assertFalse(tunnel.reaches({"http_port": 80, "host": carried["host"]}, tunnel.body("sha", tunnel.target(OWN))))
		self.assertFalse(tunnel.reaches({"http_port": 8080, "host": carried["host"]}, carried))


class Via(unittest.TestCase):
	def test_tunnel_via_names_another_node_carrying_its_own(self):
		nodes = {"tyo": {}, "sha": {"tunnel_via": "tyo"}}
		self.assertEqual(tunnel.via_refused(nodes), [])
		for via in ("sha", "lax"):
			self.assertEqual(len(tunnel.via_refused({"tyo": {}, "sha": {"tunnel_via": via}})), 1, via)
		chained = {"tyo": {"tunnel_via": "nrt"}, "nrt": {}, "sha": {"tunnel_via": "tyo"}}
		self.assertEqual(len(tunnel.via_refused(chained)), 1)


if __name__ == "__main__":
	unittest.main()
