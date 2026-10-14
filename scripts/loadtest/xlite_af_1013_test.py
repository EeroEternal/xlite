#!/usr/bin/env python3
"""Xlite-only scope and cleanup regressions, no Docker or network."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

import xlite_af_1013 as driver


class XliteOnlyTests(unittest.TestCase):
    def test_complete_scope_without_newapi(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = object.__new__(driver.XliteRig)
            instance.root = Path(directory)
            instance.results = instance.root / "results"
            instance.results.mkdir()
            instance.names = {"xlite": "gateway"}
            instance.groups = driver.shared.ORDER
            instance.k6_memory_limit = None
            for name in ["bootstrap", "smoke-all"]:
                (instance.results / (name + ".json")).write_text("{}")
            for method in ["verify_provenance", "start_farm", "configure", "reset_gateway",
                           "start_gateway", "stop"]:
                setattr(instance, method, Mock())
            instance.provenance = Mock(return_value={})
            calls = []

            def stage(gateway, group, vus, seconds, track, snapshot):
                calls.append((gateway, group, vus, seconds, track))
                return {"valid": True, "label": f"{track}-{gateway}-{group}-{vus}"}

            instance.stage = stage
            with patch.object(driver.time, "sleep"):
                instance.run_matrix()
            formal = [call for call in calls if call[-1] == "formal"]
            preflight = [call for call in calls if call[-1] == "preflight"]
            self.assertEqual(len(formal), 36)
            self.assertEqual(len(preflight), 6)
            self.assertTrue(all(call[0] == "xlite" for call in formal))
            self.assertEqual({call[1] for call in formal}, set("ABCDEF"))
            self.assertEqual({call[2] for call in formal}, set(driver.shared.LEVELS))
            self.assertTrue(all(call[0] == "direct" and call[2] == 18000 for call in preflight))
            done = json.loads((instance.results / "DONE.json").read_text())
            self.assertEqual((done["rows"], done["formal_stages"]), (42, 36))

    def test_log_timeout_does_not_block_other_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = object.__new__(driver.XliteRig)
            instance.results = Path(directory)
            instance.live = ["gateway", "farm"]
            instance.started_shards = set()
            instance.owned = Mock(return_value=True)

            def command(args, **kwargs):
                if args[:2] == ["docker", "logs"]:
                    raise subprocess.TimeoutExpired(args, 30)
                return subprocess.CompletedProcess(args, 0)

            with patch.object(driver.rig, "command", side_effect=command):
                instance.cleanup()
            self.assertEqual(instance.live, [])
            self.assertEqual(len(list(instance.results.glob("*.log-collection-error.json"))), 2)

    def test_soft_limit_passed_without_changing_payload_or_stage(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = object.__new__(driver.XliteRig)
            instance.private = Path(directory)
            instance.k6_memory_limit = "9GiB"
            snapshot = {}
            with patch.object(driver.shared.MatchedRig, "stage", return_value={"valid": True}) as stage:
                instance.stage("direct", "D", 18000, 180, "preflight", snapshot)
                stage.assert_called_once_with("direct", "D", 18000, 180, "preflight", snapshot)
            env = instance.private / "client-direct.env"
            self.assertEqual(env.read_text(), "TOKEN=sk-loadtest\nGOMEMLIMIT=9GiB\n")
            self.assertEqual(env.stat().st_mode & 0o777, 0o600)
            env.write_text("changed")
            with self.assertRaises(RuntimeError):
                instance.stage("direct", "D", 18000, 180, "preflight", snapshot)


if __name__ == "__main__":
    unittest.main()
