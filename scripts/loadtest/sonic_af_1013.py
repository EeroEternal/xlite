#!/usr/bin/env python3
"""Isolated matched Sonic/xlite A-F rig; never touches unlabelled containers."""

import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import http.cookiejar
import io
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import sqlite3
import subprocess
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request

import single_1013 as rig


PROFILES = {"A": (100, 50, 300), "B": (500, 10, 300), "C": (1200, 50, 640),
            "D": (6000, 10, 640), "E": (250, 20, 300), "F": (3000, 20, 640)}
ORDER = ["B", "A", "E", "C", "D", "F"]
LEVELS = [1000, 2000, 5000, 10000, 15000, 18000]
PORTS = list(range(43201, 43225))
GATEWAY_PORTS = {"xlite": 18881, "newapi": 18880}
K6_IMAGE = "sha256:e3721c99813c9c2a2224c9b9e254a2c2b7425a01156ec91cc0f72757eb47c953"
RUNTIME_IMAGE = "sha256:3f35ba349fbe3103d47f9f1fa96edb5c81dd7364a5ed032e15e221cdcb3e5337"
OWNER_LABEL = "xlite.bench.root"


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def private_file(path, text):
    with path.open("x") as stream:
        os.chmod(path, 0o600)
        stream.write(text)


@contextmanager
def exclusive_executor(root):
    with (root / "private" / "executor.lock").open("a") as lock:
        os.chmod(lock.name, 0o600)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("Another executor owns this task root; leaving its work untouched") from None
        yield


def http_json(url, payload=None, *, opener=None, headers=None, method=None):
    request = urllib.request.Request(
        url, data=None if payload is None else json.dumps(payload).encode(), method=method,
        headers={"Content-Type": "application/json", **(headers or {})})
    open_request = opener.open if opener else urllib.request.urlopen
    with open_request(request, timeout=30) as response:
        return json.loads(response.read())


def validate_sse(body, out_tokens):
    text = body.decode()
    frames = [line[6:] for line in text.splitlines() if line.startswith("data: ")]
    if not frames or frames[-1] != "[DONE]" or frames.count("[DONE]") != 1:
        raise RuntimeError("SSE terminal marker invalid")
    objects = [json.loads(frame) for frame in frames[:-1]]
    contents, timestamps = [], []
    stop, usage = False, None
    for item in objects:
        for choice in item.get("choices", []):
            content = choice.get("delta", {}).get("content")
            if content:
                if not isinstance(content, str):
                    raise RuntimeError("SSE content must be text")
                contents.append(content)
                stamp = item.get("ts")
                if isinstance(stamp, (int, float)):
                    timestamps.append(stamp)
            stop |= choice.get("finish_reason") == "stop"
        if item.get("usage") is not None:
            usage = item["usage"].get("completion_tokens")
    if len(contents) != out_tokens or not stop or usage != out_tokens:
        raise RuntimeError(f"SSE content/stop/usage invalid: {len(contents)}/{stop}/{usage}")
    if len(re.findall(r'"content"\s*:\s*"[^"]', text)) != out_tokens:
        raise RuntimeError("SSE regex sampling fingerprint differs from semantic smoke")
    if len(frames) < out_tokens + 2 or len(frames) > out_tokens + 5:
        raise RuntimeError("Unexpected SSE framing")
    return {"bytes": len(body), "data_lines": len(frames), "content_frames": len(contents),
            "content_sha256": hashlib.sha256("".join(contents).encode()).hexdigest(),
            "timestamps": len(timestamps), "completion_tokens": usage,
            "mock_span_ms": timestamps[-1] - timestamps[0] if len(timestamps) == out_tokens else None}


def smoke(url, out_tokens, token):
    data = {"model": "mock-model", "stream": True, "max_tokens": out_tokens,
            "messages": [{"role": "user", "content": "mock input text. " * 25}],
            "stream_options": {"include_usage": True}}
    request = urllib.request.Request(url, data=json.dumps(data).encode(),
                                    headers={"Content-Type": "application/json",
                                             "Authorization": "Bearer " + token})
    started = time.monotonic()
    with urllib.request.urlopen(request, timeout=600) as response:
        header_ms = (time.monotonic() - started) * 1000
        first = response.read(1)
        first_body_ms = (time.monotonic() - started) * 1000
        body = first + response.read()
        if response.status != 200:
            raise RuntimeError("Smoke HTTP status failed")
    result = validate_sse(body, out_tokens)
    result.update(headers_ms=header_ms, first_body_byte_ms=first_body_ms,
                  elapsed_s=time.monotonic() - started)
    return result


class MatchedRig:
    def __init__(self, root):
        self.root = root.resolve()
        self.results = self.root / "results"
        self.private = self.root / "private"
        self.prefix = self.root.name[len("xlite-"):]
        self.network = self.prefix + "-network"
        self.farm = self.prefix + "-farm"
        self.names = {key: self.prefix + "-" + key + "-gateway" for key in GATEWAY_PORTS}
        self.k6_names = [self.prefix + "-k6-" + str(i) for i in range(16)]
        self.image = RUNTIME_IMAGE
        for image in [RUNTIME_IMAGE, K6_IMAGE]:
            if rig.capture(["docker", "image", "inspect", "--format", "{{.Id}}", image]) != image:
                raise RuntimeError("Pinned image unavailable")
        self.live = []
        self.started_shards = set()
        self.secret_values = []
        self.addresses = {}

    def provenance(self):
        value = {"image": self.image, "k6_image": K6_IMAGE,
                 "driver_sha256": rig.sha(Path(__file__)),
                 "helper_sha256": rig.sha(Path(rig.__file__)),
                 "script_sha256": rig.sha(self.root / "sonic_af_1013.js"),
                 "catalog_source_sha256": rig.sha(self.root / "mock-24.yaml"),
                 "binary_sha256": {name: rig.sha(self.root / "bin" / name)
                                   for name in ["xlite", "mock-llm", "new-api-sonic"]}}
        for gateway in GATEWAY_PORTS:
            value[gateway + "_env_sha256"] = rig.sha(self.private / (gateway + ".env"))
            seed = self.private / (gateway + "-seed")
            value[gateway + "_seed_sha256"] = {
                path.name: rig.sha(path) for path in sorted(seed.iterdir())
                if path.is_file() and (path.name.startswith("newapi.db") or
                                       path.name.startswith("catalog.db"))}
        return value

    def verify_provenance(self, snapshot):
        if snapshot.get("provenance") != self.provenance():
            raise RuntimeError("Frozen inputs changed; require a fresh bootstrap/smoke run")

    def base(self, name, port):
        if name not in self.addresses:
            if not self.owned(name):
                raise RuntimeError("Unknown task container address")
            networks = json.loads(rig.capture(["docker", "inspect", "--format",
                                              "{{json .NetworkSettings.Networks}}", name]))
            address = networks[self.network]["IPAddress"]
            if not ipaddress.ip_address(address).is_private:
                raise RuntimeError("Expected private task network address")
            self.addresses[name] = address
        return f"http://{self.addresses[name]}:{port}"

    def owned(self, name, kind="container"):
        result = rig.command(["docker", kind, "inspect", "--format",
                              '{{index .Labels "' + OWNER_LABEL + '"}}' if kind == "network" else
                              '{{index .Config.Labels "' + OWNER_LABEL + '"}}', name],
                             check=False, capture_output=True, timeout=15)
        if result.returncode != 0:
            return False
        if result.stdout.strip() != str(self.root):
            raise RuntimeError(f"Refusing to touch foreign {kind} {name}")
        if kind == "container":
            image = rig.capture(["docker", "inspect", "--format", "{{.Image}}", name])
            expected = K6_IMAGE if name in self.k6_names else self.image
            if image != expected:
                raise RuntimeError(f"Pinned container image mismatch: {name}")
        return True

    def start_existing(self, name):
        if not self.owned(name):
            return False
        state = rig.capture(["docker", "inspect", "--format", "{{.State.Running}}", name])
        if state == "true":
            raise RuntimeError(f"Existing active task container {name}; do not overlap executors")
        rig.capture(["docker", "start", name])
        self.addresses.pop(name, None)
        self.live.append(name)
        return True

    def docker_start(self, name, args):
        if self.owned(name):
            raise RuntimeError(f"Container already exists: {name}")
        rig.capture(["docker", "run", "-d", "--name", name, "--label",
                     OWNER_LABEL + "=" + str(self.root), "--network", self.network,
                     "--user", f"{os.getuid()}:{os.getgid()}",
                     "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                     "--ulimit", "nofile=1048576:1048576", *args])
        self.live.append(name)

    def wait(self, url):
        for _ in range(60):
            try:
                return http_json(url)
            except (OSError, urllib.error.URLError, json.JSONDecodeError):
                time.sleep(1)
        raise RuntimeError(f"Health check failed: {url}")

    def start_farm(self):
        if not self.owned(self.network, "network"):
            rig.capture(["docker", "network", "create", "--internal", "--label",
                         OWNER_LABEL + "=" + str(self.root), self.network])
        if not self.start_existing(self.farm):
            farm_script = self.root / "farm.sh"
            private_file(farm_script, "#!/bin/sh\nset -eu\n"
                         "for p in $(seq 43201 43224); do\n"
                         "PORT=$p TOKIO_WORKER_THREADS=2 MOCK_TTFT=500 MOCK_OUT_TOKENS=500 "
                         "MOCK_TOKEN_INTERVAL=10 MOCK_BATCH=1 /usr/local/bin/mock-llm &\n"
                         "done\nwait\n")
            args = ["--network-alias", "af-mock", "--cpuset-cpus", "48-63,112-127",
                    "--cpus", "28", "--memory", "64g", "-v", f"{farm_script}:/farm.sh:ro"]
            self.docker_start(self.farm, [*args, self.image, "sh", "/farm.sh"])
        for port in PORTS:
            self.wait(self.base(self.farm, port) + "/health")

    def configure(self, group):
        tokens, interval, _ = PROFILES[group]
        expected = {"ttft": 500, "out_tokens": tokens, "token_interval": interval,
                    "batch": 1, "mix": 0, "mode": 0, "max_duration": 0, "total_delay": 0}
        configs = []
        for port in PORTS:
            value = http_json(self.base(self.farm, port) + "/_config?" +
                              urllib.parse.urlencode(expected), {})
            if any(value.get(key) != wanted for key, wanted in expected.items()):
                raise RuntimeError(f"Mock configuration mismatch on {port}: {value}")
            configs.append({"port": port, "config": value})
        write_json(self.results / f"mock-{group}.json", configs)

    def start_gateway(self, gateway):
        name, port = self.names[gateway], GATEWAY_PORTS[gateway]
        if not self.start_existing(name):
            env = self.private / (gateway + ".env")
            if gateway == "newapi":
                state = self.private / "newapi"
                state.mkdir()
                private_file(env, f"PORT={port}\nSQLITE_PATH=/data/newapi.db\nSQL_DSN=\nREDIS_CONN_STRING=\n"
                             "GIN_MODE=release\nGOMAXPROCS=32\nGOGC=400\nGOMEMLIMIT=24GiB\n"
                             "MEMORY_CACHE_ENABLED=true\nBATCH_UPDATE_ENABLED=true\n"
                             "BATCH_UPDATE_INTERVAL=5\nCountToken=false\nSTREAMING_FLUSH_INTERVAL=1\n"
                             "RELAY_MAX_IDLE_CONNS=100000\nRELAY_MAX_IDLE_CONNS_PER_HOST=50000\n"
                             f"SESSION_SECRET={secrets.token_urlsafe(32)}\n")
                executable = "new-api-sonic"
            else:
                state = self.private / "xlite"
                state.mkdir()
                private_file(env, f"DATABASE_URL=sqlite:/data/catalog.db\nLISTEN_HOST=0.0.0.0\nPORT={port}\n"
                             "XLITE_UPSTREAM_H2C=1\nXLITE_UPSTREAM_MAX_CONNS_PER_HOST=32\n"
                             "XLITE_CATALOG_RELOAD_SECS=0\nTOKIO_WORKER_THREADS=32\nRUST_LOG=warn\n"
                             f"ADMIN_PASSWORD={secrets.token_urlsafe(16)}\n"
                             f"ADMIN_SESSION_SECRET={secrets.token_urlsafe(32)}\n")
                source = (self.root / "mock-24.yaml").read_text()
                for old, new in zip(range(4101, 4125), PORTS):
                    source = source.replace(f"127.0.0.1:{old}", f"af-mock:{new}")
                source = source.replace("  - name: mock\n", "  - name: mock-model\n")
                source = source.replace("upstream_model: mock\n", "upstream_model: mock-model\n")
                catalog = self.private / "catalog.yaml"
                private_file(catalog, source)
                with (self.results / "catalog-import.log").open("w") as log:
                    rig.command(["docker", "run", "--rm", "--network", self.network,
                                 "--user", f"{os.getuid()}:{os.getgid()}",
                                 "--env-file", str(env), "-v", f"{state}:/data",
                                 "-v", f"{catalog}:/catalog.yaml:ro", self.image,
                                 "/usr/local/bin/xlite", "import", "/catalog.yaml"],
                                stdout=log, stderr=subprocess.STDOUT, timeout=120)
                executable = "xlite"
            self.docker_start(name, ["--cpuset-cpus", "0-15,64-79", "--cpus", "28", "--memory", "64g",
                                    "--network-alias", "af-" + gateway, "--env-file", str(env),
                                    "-v", f"{state}:/data", self.image, "/usr/local/bin/" + executable])
        self.wait(self.base(name, port) + ("/api/status" if gateway == "newapi" else "/health"))

    def stop(self, name, label):
        if name not in self.live or not self.owned(name):
            return
        rig.command(["docker", "stop", "--time", "10", name], capture_output=True, timeout=30)
        result = rig.command(["docker", "logs", name], capture_output=True, timeout=30)
        output = result.stdout + result.stderr
        for value in self.secret_values:
            if value:
                output = output.replace(value, "[REDACTED]")
        (self.results / f"{label}.{name}.log").write_text(output + "\n")
        self.live.remove(name)

    def cleanup(self):
        for name in self.started_shards:
            if self.owned(name):
                rig.command(["docker", "stop", "--time", "10", name], check=False,
                            capture_output=True, timeout=30)
        for name in list(reversed(self.live)):
            self.stop(name, "shutdown")

    def token(self, gateway):
        token = "sk-loadtest" if gateway != "newapi" else \
            json.loads((self.private / "credentials.json").read_text())["token"]
        if gateway == "newapi":
            self.secret_values.append(token)
        return token

    def bootstrap(self):
        if (self.results / "bootstrap.json").exists():
            raise RuntimeError("Already bootstrapped; refusing to overwrite")
        self.start_farm()
        self.configure("B")
        self.start_gateway("newapi")
        base = self.base(self.names["newapi"], GATEWAY_PORTS["newapi"])
        password = secrets.token_urlsafe(12)
        self.secret_values.append(password)
        private_file(self.private / "bootstrap-admin.json",
                     json.dumps({"username": "benchroot", "password": password}) + "\n")
        setup = http_json(base + "/api/setup")
        if setup.get("data", {}).get("status"):
            raise RuntimeError("Unexpected already initialized New API database")
        result = http_json(base + "/api/setup", {"username": "benchroot", "password": password,
                                               "confirmPassword": password,
                                               "SelfUseMode": True, "DemoMode": False})
        if not result.get("success"):
            raise RuntimeError(f"New API setup failed: {result.get('message')}")
        opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        result = http_json(base + "/api/user/login", {"username": "benchroot", "password": password},
                           opener=opener)
        if not result.get("success"):
            raise RuntimeError("New API login failed")
        write_json(self.results / "bootstrap-api-schema.json",
                   {"login_data_keys": sorted(result["data"])})
        login = result["data"]
        user = login.get("user", login)
        user_id = user["id"]
        headers = {"New-Api-User": str(user_id)}
        if login.get("access_token"):
            self.secret_values.append(login["access_token"])
            headers["Authorization"] = "Bearer " + login["access_token"]

        def api(path, payload=None, method=None):
            value = http_json(base + path, payload, opener=opener, headers=headers, method=method)
            if not value.get("success"):
                raise RuntimeError(f"New API {path} failed: {value.get('message')}")
            return value.get("data")

        options = {"ModelRatio": json.dumps({"mock-model": 1}),
                   "CompletionRatio": json.dumps({"mock-model": 1}),
                   "GroupRatio": json.dumps({"default": 1})}
        for key, value in options.items():
            api("/api/option/", {"key": key, "value": value}, "PUT")
        effective = {item["key"]: item["value"] for item in api("/api/option/")}
        write_json(self.results / "newapi-option-readback.json",
                   {key: effective.get(key) for key in options})
        for key, value in options.items():
            actual = effective.get(key)
            matches = actual == value
            if key.endswith("Ratio") and isinstance(actual, str):
                decoded = json.loads(actual)
                matches = all(decoded.get(name) == ratio for name, ratio in json.loads(value).items())
            if not matches:
                raise RuntimeError(f"New API option {key} not verified")
        api("/api/user/", {"id": user_id, "username": "benchroot", "display_name": "benchroot",
                           "role": 100, "status": 1, "group": "default",
                           "quota": 1000000000000000}, "PUT")
        for index, port in enumerate(PORTS):
            api("/api/channel/", {"mode": "single", "channel": {
                "type": 1, "name": f"af-mock-{index}", "key": "mock",
                "base_url": f"http://af-mock:{port}", "models": "mock-model",
                "group": "default", "status": 1, "weight": 1, "priority": 0}})
        api("/api/token/", {"name": "af-bench", "expired_time": -1, "unlimited_quota": True,
                           "remain_quota": 1000000000000, "model_limits_enabled": False,
                           "models": "", "group": "default"})
        listing = api("/api/token/?p=0&page_size=100")
        tokens = listing["items"] if isinstance(listing, dict) else listing
        created = next(item for item in tokens if item["name"] == "af-bench")
        key = created.get("key")
        if not key or "*" in key:
            key_result = api(f"/api/token/{created['id']}/key", {}, "POST")
            key = key_result["key"] if isinstance(key_result, dict) else key_result
        if not key or "*" in key:
            raise RuntimeError("New API did not return usable test token")
        token = key if key.startswith("sk-") else "sk-" + key
        self.secret_values += [key, token]
        private_file(self.private / "credentials.json",
                     json.dumps({"password": password, "token": token}) + "\n")
        self.stop(self.names["newapi"], "fixture-create")
        # This fork does not apply quota through the generic user update API.
        # Modify only our disposable fixture, offline, then rebuild its memory cache.
        with sqlite3.connect(self.private / "newapi" / "newapi.db") as database:
            changed = database.execute(
                "UPDATE users SET quota=? WHERE id=? AND username=?",
                (1000000000000000, user_id, "benchroot")).rowcount
            quota = database.execute("SELECT quota FROM users WHERE id=?", (user_id,)).fetchone()
            channels = database.execute(
                'SELECT COUNT(*) FROM abilities WHERE "group"=? AND model=? AND enabled=1',
                ("default", "mock-model")).fetchone()[0]
            if changed != 1 or quota != (1000000000000000,) or channels != 24:
                raise RuntimeError("Offline isolated quota/channel fixture verification failed")
        database.close()
        self.start_gateway("newapi")
        self.start_gateway("xlite")
        views = {"direct": smoke(self.base(self.farm, PORTS[0]) + "/v1/chat/completions", 500, "sk-loadtest")}
        for gateway, port in GATEWAY_PORTS.items():
            views[gateway] = smoke(self.base(self.names[gateway], port) + "/v1/chat/completions",
                                   500, self.token(gateway))
            if views[gateway]["content_sha256"] != views["direct"]["content_sha256"]:
                raise RuntimeError(f"{gateway} smoke content changed")
        for gateway in GATEWAY_PORTS:
            self.stop(self.names[gateway], "bootstrap")
            shutil.copytree(self.private / gateway, self.private / (gateway + "-seed"))
        write_json(self.results / "bootstrap.json", {
            "epoch": time.time(), "image": self.image, "k6_image": K6_IMAGE,
            "k6_version": rig.capture(["docker", "run", "--rm", "--network", "none", K6_IMAGE, "version"]),
            "binary_sha256": {key: rig.sha(self.root / "bin" / key)
                              for key in ["xlite", "mock-llm", "new-api-sonic"]},
            "newapi_options_verified": options, "newapi_status": "fresh SQLite, 24 channels, generated token",
            "newapi_user_quota_seed": 1000000000000000,
            "newapi_count_token": "CountToken=false env requested; not exposed by option API, effect unverified",
            "smoke": views, "provenance": self.provenance()})
        print("BOOTSTRAP PASS " + json.dumps(views), flush=True)

    def reset_gateway(self, gateway):
        # Only agent-created disposable active state is replaced; seed is never modified.
        active = self.private / gateway
        if self.names[gateway] in self.live:
            raise RuntimeError("Cannot reset live database")
        database = "newapi.db" if gateway == "newapi" else "catalog.db"
        seed = self.private / (gateway + "-seed")
        for name in [database, database + "-wal", database + "-shm", database + "-journal"]:
            path = active / name
            if path.exists():
                if not path.is_file() or path.is_symlink():
                    raise RuntimeError("Unexpected disposable SQLite path type")
                path.unlink()
            if (seed / name).exists():
                shutil.copy2(seed / name, path)
        # Application-generated logs remain intact; never delete arbitrary state files.

    def all_smokes(self):
        if (self.results / "smoke-all.json").exists():
            raise RuntimeError("Smoke snapshot exists; refusing to overwrite")
        self.verify_provenance(json.loads((self.results / "bootstrap.json").read_text()))
        self.start_farm()
        groups = {}
        for group in ORDER:
            self.configure(group)
            out_tokens = PROFILES[group][0]
            views = {"direct": smoke(self.base(self.farm, PORTS[0]) + "/v1/chat/completions",
                                     out_tokens, "sk-loadtest")}
            for gateway, port in GATEWAY_PORTS.items():
                self.reset_gateway(gateway)
                self.start_gateway(gateway)
                views[gateway] = smoke(self.base(self.names[gateway], port) + "/v1/chat/completions",
                                       out_tokens, self.token(gateway))
                if views[gateway]["content_sha256"] != views["direct"]["content_sha256"]:
                    raise RuntimeError(f"{group} {gateway} content differs from direct")
                self.stop(self.names[gateway], "smoke-" + group)
            groups[group] = views
            print("SMOKE PASS " + group + " " + json.dumps(views), flush=True)
        timing = all(view["timestamps"] == PROFILES[group][0]
                     for group, views in groups.items() for view in views.values())
        write_json(self.results / "smoke-all.json", {
            "epoch": time.time(), "driver_sha256": rig.sha(Path(__file__)),
            "timestamp_diagnostics": timing, "groups": groups,
            "provenance": self.provenance(),
            "timestamp_note": "enabled for both" if timing else
            "disabled for both in matched formal line; absence is not zero"})

    def classify(self, summary, vus, elapsed, seconds, out_tokens, timing):
        row = rig.classify(summary, vus, 0.5, elapsed, seconds)
        for name in ["transport_errors", "non200_responses", "body_errors"]:
            row[name] = rig.values(summary, name).get("count", 0)
            if row[name]:
                row["invalid_reasons"].append(f"{name} nonzero")
        if row["failure_rate"] != 0:
            row["invalid_reasons"].append("HTTP failures must be zero")
        expected = row["body_samples"] * min(64, out_tokens - 1) if timing else 0
        if row["chunk_samples"] != expected:
            row["invalid_reasons"].append("chunk sample fingerprint mismatch")
        diagnostics = rig.values(summary, "timestamp_diagnostic_bodies").get("count", 0)
        if diagnostics != (row["body_samples"] if timing else 0):
            row["invalid_reasons"].append("timestamp diagnostic fingerprint mismatch")
        row["valid"] = not row["invalid_reasons"]
        return row

    def stage(self, gateway, group, vus, seconds, track, smoke_snapshot):
        label = f"{track}-{gateway}-{group}-{vus}"
        if (self.results / (label + ".row.json")).exists():
            raise RuntimeError(f"Stage already exists: {label}")
        out_tokens = PROFILES[group][0]
        view = smoke_snapshot["groups"][group][gateway]
        timing = smoke_snapshot["timestamp_diagnostics"]
        target = ",".join(f"http://af-mock:{port}/v1/chat/completions" for port in PORTS) \
            if gateway == "direct" else f"http://af-{gateway}:{GATEWAY_PORTS[gateway]}/v1/chat/completions"
        env = self.private / ("client-" + gateway + ".env")
        if not env.exists():
            private_file(env, "TOKEN=" + self.token(gateway) + "\n")
        log_handles, processes, summaries = [], [], []
        print(f"START {label} sample=0.5 seconds={seconds} epoch={time.time()}", flush=True)
        started = time.monotonic()
        previous = {}
        rig.GATEWAY_PORT = GATEWAY_PORTS.get(gateway, 18881)
        try:
            for index, name in enumerate(self.k6_names):
                if self.owned(name):
                    raise RuntimeError("Previous k6 shard still exists")
                shard_vus = vus // 16 + int(index < vus % 16)
                summary = self.results / f"{label}.shard-{index}.summary.json"
                summaries.append(summary)
                log = (self.results / f"{label}.shard-{index}.log").open("x")
                log_handles.append(log)
                command = ["docker", "run", "--rm", "--name", name, "--label",
                           OWNER_LABEL + "=" + str(self.root), "--network", self.network,
                           "--user", f"{os.getuid()}:{os.getgid()}", "--cpuset-cpus", "16-47,80-111",
                           "--cpus", "3.5", "--memory", "12g", "--cap-drop", "ALL",
                           "--security-opt", "no-new-privileges", "--ulimit", "nofile=1048576:1048576",
                           "-v", f"{self.root}:/scripts:ro", "-v", f"{self.results}:/results",
                           "--env-file", str(env), "-e", "GOMAXPROCS=4", "-e", "GOGC=400",
                           K6_IMAGE, "run", "--quiet", "--compatibility-mode=base",
                           "--include-system-env-vars=true", "-e", f"VUS={shard_vus}",
                           "-e", "WARMUP=30", "-e", f"DURATION_SECONDS={seconds}",
                           "-e", "KEEP_SAMPLE=0.5", "-e", "CHUNK_MAX=64",
                           "-e", f"OUT_TOKENS={out_tokens}", "-e", f"EXPECTED_DATA_LINES={view['data_lines']}",
                           "-e", f"TIMESTAMP_DIAGNOSTICS={str(timing).lower()}", "-e", f"TARGET_URLS={target}",
                           "--summary-export", f"/results/{summary.name}", "/scripts/sonic_af_1013.js"]
                processes.append(subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT))
                self.started_shards.add(name)
            with (self.results / f"{label}.resources.jsonl").open("x") as stream:
                while any(process.poll() is None for process in processes):
                    time.sleep(5)
                    temporary = io.StringIO()
                    rig.monitor([self.farm, *([self.names[gateway]] if gateway != "direct" else []),
                                 *self.k6_names], temporary, time.monotonic() - started, previous)
                    record = json.loads(temporary.getvalue())
                    record["gateway_established_connections"] = None
                    record["connections_note"] = "host ss does not count private container network namespace"
                    stream.write(json.dumps(record) + "\n")
                    stream.flush()
                    if time.monotonic() - started > 30 + seconds + 630:
                        raise RuntimeError("Stage exceeded deadline")
            return_codes = [process.wait() for process in processes]
        finally:
            for name, process in zip(self.k6_names, processes):
                if process.poll() is None and self.owned(name):
                    rig.command(["docker", "stop", "--time", "10", name], check=False,
                                capture_output=True, timeout=30)
                    process.wait(timeout=30)
            for log in log_handles:
                log.close()
            self.started_shards.clear()
        elapsed = time.monotonic() - started
        rows = []
        for index, (summary, code) in enumerate(zip(summaries, return_codes)):
            if not summary.exists():
                raise RuntimeError(f"Missing summary {summary.name}, exit={code}")
            n = vus // 16 + int(index < vus % 16)
            row = self.classify(json.loads(summary.read_text()), n, elapsed, seconds, out_tokens, timing)
            if code != 0:
                row["invalid_reasons"].append(f"nonzero k6 exit {code}")
                row["valid"] = False
            row.update(shard=index, vus=n, exit_code=code)
            rows.append(row)
        row = rig.aggregate(rows)
        row.update(label=label, gateway=gateway, group=group, vus=vus, track=track,
                   sample=0.5, duration_s=seconds, elapsed_s=elapsed, epoch=time.time(),
                   timestamp_diagnostics=timing, data_lines_expected=view["data_lines"])
        if gateway == "direct":
            ideal_ms = 500 + PROFILES[group][0] * PROFILES[group][1]
            if row["estimated_active"] is None or row["estimated_active"] < vus * 0.85 or \
                    row["http_duration_ms"].get("avg", float("inf")) > ideal_ms * 1.28:
                row["invalid_reasons"].append("Direct mock/client capacity gate failed")
                row["valid"] = False
        write_json(self.results / (label + ".row.json"), row)
        print("END " + json.dumps(row), flush=True)
        return row

    def run_matrix(self):
        smoke_snapshot = json.loads((self.results / "smoke-all.json").read_text())
        if (self.results / "matrix-env.json").exists():
            raise RuntimeError("Matrix already started; use a fresh run to avoid stitching")
        self.verify_provenance(json.loads((self.results / "bootstrap.json").read_text()))
        self.verify_provenance(smoke_snapshot)
        write_json(self.results / "matrix-env.json", {
            "epoch": time.time(), "hostname": socket.gethostname(), "profiles": PROFILES,
            "order": ORDER, "levels": LEVELS, "sample": 0.5, "warmup_s": 30, "graceful_stop_s": 600,
            "driver_sha256": rig.sha(Path(__file__)), "helper_sha256": rig.sha(Path(rig.__file__)),
            "script_sha256": rig.sha(self.root / "sonic_af_1013.js"), "image": self.image,
            "k6_image": K6_IMAGE, "timestamp_diagnostics": smoke_snapshot["timestamp_diagnostics"],
            "cpu_masks": {"gateway": "0-15,64-79", "k6": "16-47,80-111", "farm": "48-63,112-127"},
            "cpu_quotas": {"gateway": 28, "k6": 56, "farm": 28},
            "memory_gib": {"gateway": 64, "k6": 192, "farm": 64},
            "upstream": {"xlite": "h2c, max32 connections/host", "newapi": "native HTTP transport"},
            "database": "fresh private SQLite seed before every gateway stage",
            "topology": "all three roles in one private internal Docker bridge, no published ports",
            "provenance": self.provenance(),
            "client_body_validation": "regex fingerprint; full JSON/content hash in 100% semantic smoke",
            "baseline_note": "not latest xlite; not controlled isolation of Sonic alone"})
        self.start_farm()
        rows = []
        for group_index, group in enumerate(ORDER):
            self.configure(group)
            seconds = 60 if PROFILES[group][2] == 300 else 180
            preflight = self.stage("direct", group, 18000, seconds, "preflight", smoke_snapshot)
            rows.append(preflight)
            write_json(self.results / "progress.json", rows)
            if not preflight["valid"]:
                raise RuntimeError(f"Preflight failed for {group}; not running invalid load matrix")
            for level_index, vus in enumerate(LEVELS):
                pair = ["xlite", "newapi"] if (group_index + level_index) % 2 == 0 else ["newapi", "xlite"]
                for gateway in pair:
                    self.reset_gateway(gateway)
                    self.start_gateway(gateway)
                    row = self.stage(gateway, group, vus, PROFILES[group][2], "formal", smoke_snapshot)
                    rows.append(row)
                    write_json(self.results / "progress.json", rows)
                    self.stop(self.names[gateway], row["label"])
                    if not row["valid"]:
                        raise RuntimeError(f"Formal stage failed: {row['label']}; matrix stopped")
                    time.sleep(10)
        write_json(self.results / "DONE.json", {"epoch": time.time(), "rows": len(rows),
                                              "formal_stages": sum(row["track"] == "formal" for row in rows)})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--phase", choices=["bootstrap", "smoke", "run"], required=True)
    args = parser.parse_args()
    root = args.root.resolve()
    if not re.fullmatch(r"xlite-sonic-af-1013-r[1-9][0-9]*", root.name) or not (root / "private").is_dir():
        raise RuntimeError("Unexpected task root")
    with exclusive_executor(root):
        instance = MatchedRig(root)

        def interrupted(signum, frame):
            raise RuntimeError(f"Interrupted by signal {signum}")

        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        try:
            if args.phase == "bootstrap":
                instance.bootstrap()
            elif args.phase == "smoke":
                instance.all_smokes()
            else:
                instance.run_matrix()
        except Exception as exc:
            message = str(exc)
            trace = traceback.format_exc()
            for value in instance.secret_values:
                if value:
                    message = message.replace(value, "[REDACTED]")
                    trace = trace.replace(value, "[REDACTED]")
            write_json(instance.results / f"FAILED-{args.phase}.json",
                       {"epoch": time.time(), "error": message, "traceback": trace})
            raise RuntimeError(message) from None
        finally:
            instance.cleanup()


if __name__ == "__main__":
    main()
