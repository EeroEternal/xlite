#!/usr/bin/env python3
"""Fresh xlite-only A-F matrix; no New API process or database is started."""
import argparse
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time

import sonic_af_1013 as shared
import single_1013 as rig


class XliteRig(shared.MatchedRig):
    def __init__(self, root, groups=None, k6_memory_limit=None):
        super().__init__(root)
        self.groups = list(groups or shared.ORDER)
        self.k6_memory_limit = k6_memory_limit

    def provenance(self):
        return {
            "image": self.image, "k6_image": shared.K6_IMAGE,
            "groups": self.groups, "k6_memory_limit": self.k6_memory_limit,
            "driver_sha256": rig.sha(Path(__file__)),
            "shared_driver_sha256": rig.sha(Path(shared.__file__)),
            "helper_sha256": rig.sha(Path(rig.__file__)),
            "script_sha256": rig.sha(self.root / "sonic_af_1013.js"),
            "catalog_source_sha256": rig.sha(self.root / "mock-24.yaml"),
            "binary_sha256": {name: rig.sha(self.root / "bin" / name) for name in ["xlite", "mock-llm"]},
            "xlite_env_sha256": rig.sha(self.private / "xlite.env"),
            "xlite_seed_sha256": {
                path.name: rig.sha(path) for path in sorted((self.private / "xlite-seed").iterdir())
                if path.is_file() and path.name.startswith("catalog.db")}}

    def stop(self, name, label):
        if name not in self.live or not self.owned(name):
            return
        rig.command(["docker", "stop", "--time", "10", name], capture_output=True, timeout=30)
        self.live.remove(name)
        # Preserve a bounded diagnostic tail, never let log collection prevent cleanup.
        destination = self.results / f"{label}.{name}.tail10000.log"
        try:
            with destination.open("x") as log:
                rig.command(["docker", "logs", "--tail", "10000", name],
                            stdout=log, stderr=subprocess.STDOUT, timeout=30)
        except (subprocess.TimeoutExpired, subprocess.CalledProcessError) as exc:
            shared.write_json(self.results / f"{label}.{name}.log-collection-error.json",
                              {"error": str(exc), "complete_log_collected": False})

    def cleanup(self):
        errors = []
        for name in self.started_shards:
            try:
                if self.owned(name):
                    rig.command(["docker", "stop", "--time", "10", name], check=False,
                                capture_output=True, timeout=30)
            except Exception as exc:
                errors.append({"name": name, "error": str(exc)})
        for name in list(reversed(self.live)):
            try:
                self.stop(name, "shutdown")
            except Exception as exc:
                errors.append({"name": name, "error": str(exc)})
        if errors:
            shared.write_json(self.results / "cleanup-errors.json", errors)

    def bootstrap(self):
        if (self.results / "bootstrap.json").exists():
            raise RuntimeError("Existing bootstrap; refusing overwrite")
        self.start_farm()
        self.configure("B")
        self.start_gateway("xlite")
        views = {
            "direct": shared.smoke(self.base(self.farm, shared.PORTS[0]) + "/v1/chat/completions",
                                   500, "sk-loadtest"),
            "xlite": shared.smoke(self.base(self.names["xlite"], shared.GATEWAY_PORTS["xlite"]) +
                                  "/v1/chat/completions", 500, "sk-loadtest")}
        if views["direct"]["content_sha256"] != views["xlite"]["content_sha256"]:
            raise RuntimeError("Bootstrap content mismatch")
        self.stop(self.names["xlite"], "bootstrap")
        shutil.copytree(self.private / "xlite", self.private / "xlite-seed")
        shared.write_json(self.results / "bootstrap.json",
                          {"epoch": time.time(), "scope": "xlite only", "smoke": views,
                           "provenance": self.provenance()})
        print("BOOTSTRAP PASS " + json.dumps(views), flush=True)

    def all_smokes(self):
        if (self.results / "smoke-all.json").exists():
            raise RuntimeError("Existing smoke snapshot; refusing overwrite")
        self.verify_provenance(json.loads((self.results / "bootstrap.json").read_text()))
        self.start_farm()
        groups = {}
        for group in self.groups:
            self.configure(group)
            tokens = shared.PROFILES[group][0]
            direct = shared.smoke(self.base(self.farm, shared.PORTS[0]) + "/v1/chat/completions",
                                  tokens, "sk-loadtest")
            self.reset_gateway("xlite")
            self.start_gateway("xlite")
            gateway = shared.smoke(self.base(self.names["xlite"], shared.GATEWAY_PORTS["xlite"]) +
                                   "/v1/chat/completions", tokens, "sk-loadtest")
            if direct["content_sha256"] != gateway["content_sha256"]:
                raise RuntimeError(f"{group} content mismatch")
            self.stop(self.names["xlite"], "smoke-" + group)
            groups[group] = {"direct": direct, "xlite": gateway}
            print("SMOKE PASS " + group + " " + json.dumps(groups[group]), flush=True)
        timing = all(view["timestamps"] == shared.PROFILES[group][0]
                     for group, views in groups.items() for view in views.values())
        shared.write_json(self.results / "smoke-all.json",
                          {"epoch": time.time(), "scope": "xlite only", "groups": groups,
                           "timestamp_diagnostics": timing, "provenance": self.provenance()})

    def stage(self, gateway, group, vus, seconds, track, snapshot):
        if self.k6_memory_limit:
            env = self.private / ("client-" + gateway + ".env")
            expected = "TOKEN=sk-loadtest\nGOMEMLIMIT=" + self.k6_memory_limit + "\n"
            if not env.exists():
                shared.private_file(env, expected)
            elif env.read_text() != expected:
                raise RuntimeError("Client memory/token environment changed")
        return super().stage(gateway, group, vus, seconds, track, snapshot)

    def run_matrix(self):
        if (self.results / "matrix-env.json").exists():
            raise RuntimeError("Existing matrix; refusing to stitch results")
        smoke = json.loads((self.results / "smoke-all.json").read_text())
        self.verify_provenance(json.loads((self.results / "bootstrap.json").read_text()))
        self.verify_provenance(smoke)
        shared.write_json(self.results / "matrix-env.json", {
            "epoch": time.time(), "scope": "xlite only", "profiles": shared.PROFILES,
            "levels": shared.LEVELS, "order": self.groups, "sample": 0.5,
            "k6_gogc": 400, "k6_memory_limit": self.k6_memory_limit,
            "warmup_s": 30, "graceful_stop_s": 600, "shards": 16,
            "cpu_masks": {"gateway": "0-15,64-79", "k6": "16-47,80-111", "farm": "48-63,112-127"},
            "cpu_quotas": {"gateway": 28, "k6": 56, "farm": 28},
            "memory_gib": {"gateway": 64, "k6": 192, "farm": 64},
            "topology": "private internal Docker bridge, no published ports",
            "upstream": "h2c, max32 connections/host",
            "gateway_log_capture": "bounded last10000lines; full daemon logs retained on server",
            "provenance": self.provenance()})
        self.start_farm()
        rows = []
        for group in self.groups:
            self.configure(group)
            seconds = 60 if shared.PROFILES[group][2] == 300 else 180
            row = self.stage("direct", group, 18000, seconds, "preflight", smoke)
            rows.append(row)
            shared.write_json(self.results / "progress.json", rows)
            if not row["valid"]:
                raise RuntimeError(f"{group} capacity preflight failed")
            for vus in shared.LEVELS:
                self.reset_gateway("xlite")
                self.start_gateway("xlite")
                row = self.stage("xlite", group, vus, shared.PROFILES[group][2], "formal", smoke)
                rows.append(row)
                shared.write_json(self.results / "progress.json", rows)
                self.stop(self.names["xlite"], row["label"])
                if not row["valid"]:
                    raise RuntimeError(f"{row['label']} invalid; not advancing")
                time.sleep(10)
        shared.write_json(self.results / "DONE.json",
                          {"epoch": time.time(), "rows": len(rows),
                           "formal_stages": len(self.groups) * 6, "groups": self.groups})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--phase", choices=["bootstrap", "smoke", "run", "all"], required=True)
    parser.add_argument("--groups", default="B,A,E,C,D,F", help="Whole group curves, never individual levels")
    parser.add_argument("--k6-memory-limit", choices=["9GiB"])
    args = parser.parse_args()
    root = args.root.resolve()
    if not re.fullmatch(r"xlite-sonic-af-1013-r[1-9][0-9]*", root.name) or not (root / "private").is_dir():
        raise RuntimeError("Unexpected task root")
    groups = args.groups.split(",")
    if len(set(groups)) != len(groups) or not groups or any(group not in shared.PROFILES for group in groups):
        raise RuntimeError("Invalid or duplicate groups")
    with shared.exclusive_executor(root):
        instance = XliteRig(root, groups, args.k6_memory_limit)

        def interrupted(signum, frame):
            raise RuntimeError(f"Interrupted by signal {signum}")

        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        try:
            if args.phase in ["bootstrap", "all"]:
                instance.bootstrap()
                instance.stop(instance.farm, "bootstrap")
            if args.phase in ["smoke", "all"]:
                instance.all_smokes()
                instance.stop(instance.farm, "smoke")
            if args.phase in ["run", "all"]:
                instance.run_matrix()
        except Exception as exc:
            shared.write_json(instance.results / f"FAILED-{args.phase}.json",
                              {"epoch": time.time(), "error": str(exc)})
            raise
        finally:
            instance.cleanup()


if __name__ == "__main__":
    main()
