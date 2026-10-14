#!/usr/bin/env python3
"""Deterministic positive/negative checks; no Docker or network access."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import sonic_af_1013 as matched


def body_fixture(out_tokens=3, timestamps=True):
    frames = []
    for index in range(out_tokens):
        item = {"choices": [{"delta": {"content": "abcd"}}]}
        if timestamps:
            item["ts"] = index * 10
        frames.append(json.dumps(item))
    frames.append(json.dumps({"choices": [{"delta": {}, "finish_reason": "stop"}],
                              "usage": {"completion_tokens": out_tokens}}))
    return "".join("data: " + item + "\n\n" for item in frames + ["[DONE]"]).encode()


def summary_fixture():
    metrics = {name: {"count": count} for name, count in {
        "http_reqs": 1000, "body_samples": 500, "chunk_samples_added": 1000,
        "timestamp_diagnostic_bodies": 500, "steady_successful_requests": 900}.items()}
    metrics.update(http_req_failed={"value": 0}, checks={"passes": 1500, "fails": 0},
                   http_req_duration={"avg": 5500}, iteration_duration={"avg": 5510})
    return {"metrics": metrics}


class ContractTests(unittest.TestCase):
    def test_full_semantic_smoke(self):
        result = matched.validate_sse(body_fixture(), 3)
        self.assertEqual((result["content_frames"], result["data_lines"], result["timestamps"]), (3, 5, 3))
        self.assertEqual(result["mock_span_ms"], 20)

    def test_absent_timestamps_remain_unknown(self):
        result = matched.validate_sse(body_fixture(timestamps=False), 3)
        self.assertEqual(result["timestamps"], 0)
        self.assertIsNone(result["mock_span_ms"])

    def test_semantic_failures(self):
        good = body_fixture()
        for bad in [good.replace(b"[DONE]", b"[BAD]"),
                    good.replace(b'"completion_tokens": 3', b'"completion_tokens": 2'),
                    good.replace(b'"finish_reason": "stop"', b'"finish_reason": null'),
                    good.replace(b'"ts": 10', b'"ts": INVALID'),
                    good + b"data: [DONE]\n\n", body_fixture(2)]:
            with self.subTest(body=bad):
                with self.assertRaises((RuntimeError, json.JSONDecodeError)):
                    matched.validate_sse(bad, 3)

    def classify(self, summary, elapsed=335, timing=True):
        instance = object.__new__(matched.MatchedRig)
        return instance.classify(summary, 62, elapsed, 300, 3, timing)

    def test_classifier_positive_and_disabled_diagnostics(self):
        self.assertTrue(self.classify(summary_fixture())["valid"])
        summary = summary_fixture()
        summary["metrics"]["chunk_samples_added"]["count"] = 0
        summary["metrics"]["timestamp_diagnostic_bodies"]["count"] = 0
        self.assertTrue(self.classify(summary, timing=False)["valid"])

    def test_classifier_negative_fingerprints(self):
        for metric, count in [("body_errors", 1), ("transport_errors", 1), ("non200_responses", 1),
                              ("chunk_samples_added", 999), ("body_samples", 600),
                              ("timestamp_diagnostic_bodies", 499)]:
            summary = summary_fixture()
            summary["metrics"][metric] = {"count": count}
            with self.subTest(metric=metric):
                self.assertFalse(self.classify(summary)["valid"])
        self.assertFalse(self.classify(summary_fixture(), elapsed=200)["valid"])
        summary = summary_fixture()
        summary["metrics"]["http_req_failed"]["value"] = 0.0001
        self.assertFalse(self.classify(summary)["valid"])

    def test_client_and_target_guard(self):
        script = Path(__file__).with_name("sonic_af_1013.js")
        harness = r"""
const fs = require('fs'), vm = require('vm');
const source = fs.readFileSync(process.argv[1], 'utf8');
const good = process.argv[2], bad = process.argv[3];
function run(body, overrides) {
  const counts = {}, checks = [];
  function Metric(name) { this.add = v => { counts[name] = (counts[name] || 0) + v; }; }
  const response = {status: 200, body: body, timings: {waiting: 501, duration: 5500, receiving: 4990}};
  const env = Object.assign({TARGET_URLS: 'http://af-xlite:18881/v1/chat/completions',
    VUS: '1000', OUT_TOKENS: '3', EXPECTED_DATA_LINES: '5', DURATION_SECONDS: '300',
    TOKEN: 'test-only', TIMESTAMP_DIAGNOSTICS: 'true'}, overrides || {});
  const math = Object.create(Math); math.random = () => 0;
  const modules = {'k6/http': {post: () => response},
    'k6': {check: (r,c) => Object.keys(c).forEach(k => checks.push(c[k](r))), sleep: () => {}},
    'k6/metrics': {Trend: Metric, Counter: Metric},
    'k6/execution': {scenario: {startTime: Date.now() - 60000}}};
  const context = {require: name => modules[name], __ENV: env, __VU: 1, module: {exports: {}},
    Date: Date, Math: math, Number: Number, RegExp: RegExp};
  vm.runInNewContext(source, context);
  context.module.exports.default();
  return {counts, checks};
}
let result = run(good);
if (!result.checks.every(Boolean) || result.counts.chunk_samples_added !== 2) throw Error('good body failed');
result = run(bad);
if (result.checks.every(Boolean) || result.counts.body_errors !== 1) throw Error('bad body accepted');
for (const overrides of [{VUS: '20000'}, {TARGET_URLS: 'http://example.com:80/v1/chat/completions'}]) {
  let rejected = false;
  try { run(good, overrides); } catch (e) { rejected = true; }
  if (!rejected) throw Error('unsafe target/VUS accepted');
}
"""
        subprocess.run(["node", "-e", harness, str(script), body_fixture().decode(),
                        body_fixture().decode().replace('"completion_tokens": 3', '"completion_tokens": 2')],
                       check=True, timeout=15)

    def test_exclusive_lock_refuses_second_executor(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "private").mkdir()
            with matched.exclusive_executor(root):
                with self.assertRaises(RuntimeError):
                    with matched.exclusive_executor(root):
                        self.fail("second executor acquired active lock")

    def test_rejected_executor_does_not_stop_existing_shards(self):
        instance = object.__new__(matched.MatchedRig)
        instance.started_shards = set()
        instance.live = []
        with patch.object(instance, "owned", return_value=True), \
                patch.object(matched.rig, "command") as command:
            instance.cleanup()
            command.assert_not_called()

    def test_provenance_mismatch_rejected(self):
        instance = object.__new__(matched.MatchedRig)
        with patch.object(instance, "provenance", return_value={"image": "pinned"}):
            instance.verify_provenance({"provenance": {"image": "pinned"}})
            with self.assertRaises(RuntimeError):
                instance.verify_provenance({"provenance": {"image": "retagged"}})

    def test_container_image_mismatch_rejected(self):
        instance = object.__new__(matched.MatchedRig)
        instance.root = Path("/known/task")
        instance.image = "pinned-runtime"
        instance.k6_names = []
        result = subprocess.CompletedProcess([], 0, stdout="/known/task\n")
        with patch.object(matched.rig, "command", return_value=result), \
                patch.object(matched.rig, "capture", return_value="retagged"):
            with self.assertRaises(RuntimeError):
                instance.owned("known-gateway")

    def test_sqlite_reset_preserves_generated_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            instance = object.__new__(matched.MatchedRig)
            instance.private = Path(directory)
            instance.names = {"newapi": "known-gateway"}
            instance.live = []
            active, seed = instance.private / "newapi", instance.private / "newapi-seed"
            active.mkdir(); seed.mkdir()
            (active / "logs").mkdir()
            (active / "logs" / "generated.log").write_text("preserve")
            (active / "newapi.db").write_text("used state")
            (seed / "newapi.db").write_text("fixture")
            instance.reset_gateway("newapi")
            self.assertEqual((active / "newapi.db").read_text(), "fixture")
            self.assertEqual((active / "logs" / "generated.log").read_text(), "preserve")


if __name__ == "__main__":
    unittest.main()
