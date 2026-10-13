#!/usr/bin/env python3
"""Run the isolated, loopback-only 1013 B-group rig on the Linux test server."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import secrets
import socket
import subprocess
import time
import urllib.request


PREFIX = "xlite-single-1013-r1"
FARM = PREFIX + "-farm"
GATEWAY = PREFIX + "-gateway"
K6 = PREFIX + "-k6"
PORTS = list(range(43101, 43125))
GATEWAY_PORT = 18313


def command(args, *, check=True, **kwargs):
    return subprocess.run(args, check=check, text=True, **kwargs)


def capture(args):
    return command(args, capture_output=True).stdout.strip()


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def request(url, payload=None):
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={"Content-Type": "application/json",
                                                         "Authorization": "Bearer sk-loadtest"})
    with urllib.request.urlopen(req, timeout=15) as response:
        return response.status, response.read()


def smoke(url):
    start = time.monotonic()
    status, body = request(url, {"model": "mock-model", "stream": True,
                                "messages": [{"role": "user", "content": "mock input text. " * 25}],
                                "stream_options": {"include_usage": True}})
    frames = [line[6:] for line in body.decode().splitlines() if line.startswith("data: ")]
    content = [json.loads(line) for line in frames if line != "[DONE]"]
    timestamps = [item["ts"] for item in content if "ts" in item]
    if status != 200 or len(timestamps) != 500 or frames[-1] != "[DONE]":
        raise RuntimeError("Smoke failed: status/frame count/DONE")
    return {"status": status, "bytes": len(body), "data_lines": len(frames),
            "content_frames": len(timestamps), "mock_span_ms": timestamps[-1] - timestamps[0],
            "elapsed_s": time.monotonic() - start}


def values(summary, metric):
    item = summary["metrics"].get(metric, {})
    return item.get("values", item)


def classify(summary, vus, sample, elapsed, duration_seconds=300):
    reqs = values(summary, "http_reqs").get("count", 0)
    fail = values(summary, "http_req_failed").get("value")
    if fail is None:
        fail = values(summary, "http_req_failed").get("rate")
    checks = values(summary, "checks")
    passed = checks.get("passes", 0)
    failed_checks = checks.get("fails", 0)
    samples = values(summary, "body_samples").get("count", 0)
    errors = values(summary, "body_errors").get("count", 0)
    http_avg = values(summary, "http_req_duration").get("avg")
    iter_avg = values(summary, "iteration_duration").get("avg")
    active = vus * http_avg / iter_avg if http_avg is not None and iter_avg else None
    invalid = []
    if reqs <= 0:
        invalid.append("no requests")
    if elapsed < 30 + duration_seconds - 1:
        invalid.append("incomplete requested duration")
    if fail is None or fail >= 0.01:
        invalid.append("missing/excessive HTTP failure rate")
    if errors or failed_checks:
        invalid.append("stream/status checks failed")
    if passed + failed_checks != reqs + samples:
        invalid.append("checks/request fingerprint mismatch")
    sampling_tolerance = 0.0 if sample == 1 else (
        max(0.02, 5 * math.sqrt(sample * (1 - sample) / reqs)) if reqs else None)
    if reqs and abs(samples / reqs - sample) > sampling_tolerance:
        invalid.append("body sampling fingerprint mismatch")
    return {"valid": not invalid, "invalid_reasons": invalid, "requests": reqs,
            "failure_rate": fail, "body_samples": samples, "sampling_tolerance": sampling_tolerance,
            "estimated_active": active,
            "qps_whole_run": values(summary, "http_reqs").get("rate"),
            "steady_success_qps": values(summary, "steady_successful_requests").get("count", 0) / duration_seconds,
            "ttft_ms": values(summary, "ttft_ms"),
            "http_duration_ms": values(summary, "http_req_duration"),
            "iteration_duration_ms": values(summary, "iteration_duration"),
            "gateway_overhead_ms": values(summary, "gateway_overhead_ms"),
            "chunk_samples": values(summary, "chunk_samples_added").get("count", 0)}


def aggregate(rows):
    """Merge counts/rates, never average shard quantiles."""
    total = sum(row["requests"] for row in rows)
    result = {"valid": all(row["valid"] for row in rows),
              "invalid_reasons": [reason for row in rows for reason in row["invalid_reasons"]],
              "requests": total, "body_samples": sum(row["body_samples"] for row in rows),
              "chunk_samples": sum(row["chunk_samples"] for row in rows),
              "shards": rows}
    for key in ["qps_whole_run", "steady_success_qps", "estimated_active"]:
        result[key] = (sum(row[key] for row in rows)
                       if all(row[key] is not None for row in rows) else None)
    result["failure_rate"] = (
        sum(row["failure_rate"] * row["requests"] for row in rows) / total
        if total and all(row["failure_rate"] is not None for row in rows) else None)
    for key in ["ttft_ms", "http_duration_ms", "iteration_duration_ms", "gateway_overhead_ms"]:
        stats = {}
        weight_key = "body_samples" if key == "gateway_overhead_ms" else "requests"
        weight = sum(row[weight_key] for row in rows)
        if weight and all("avg" in row[key] for row in rows):
            stats["avg"] = sum(row[key]["avg"] * row[weight_key] for row in rows) / weight
        for percentile in ["p(50)", "p(90)", "p(95)", "p(99)"]:
            numbers = [row[key][percentile] for row in rows if percentile in row[key]]
            if len(numbers) == len(rows):
                stats[percentile + "_shard_range"] = [min(numbers), max(numbers)]
        result[key] = stats
    return result


def monitor(containers, stream, elapsed, previous):
    record = {"epoch": time.time(), "elapsed_s": elapsed}
    record["containers"] = []
    try:
        states = command(["docker", "inspect", "--format",
                          "{{.Name}} {{.State.Pid}} {{.State.Running}}", *containers],
                         check=False, capture_output=True, timeout=10)
        if states.returncode:
            record["inspect_error"] = states.stderr.strip()
        for line in states.stdout.splitlines():
            name, pid, running = line.split()
            if running != "true" or pid == "0":
                continue
            item = {"name": name.lstrip("/"), "cpu_cores": None, "memory_bytes": None}
            try:
                relative = Path(f"/proc/{pid}/cgroup").read_text().strip().split("0::", 1)[1]
                cgroup = Path("/sys/fs/cgroup") / relative.lstrip("/")
                cpu = dict(line.split() for line in (cgroup / "cpu.stat").read_text().splitlines())
                usage = int(cpu["usage_usec"])
                now = time.monotonic()
                key = str(cgroup)
                if key in previous and usage >= previous[key][0]:
                    item["cpu_cores"] = (usage - previous[key][0]) / 1e6 / (now - previous[key][1])
                previous[key] = (usage, now)
                item["memory_bytes"] = int((cgroup / "memory.current").read_text())
                item["cpu_stat"] = {key: int(value) for key, value in cpu.items()}
                item["memory_events"] = dict(
                    line.split() for line in (cgroup / "memory.events").read_text().splitlines())
            except (OSError, ValueError, IndexError, KeyError) as exc:
                item["error"] = str(exc)
            record["containers"].append(item)
    except subprocess.TimeoutExpired:
        record["inspect_error"] = "Docker state lookup timed out; container resources unknown"
    record["loadavg"] = Path("/proc/loadavg").read_text().strip()
    record["meminfo"] = {line.split(":")[0]: line.split(":")[1].strip()
                         for line in Path("/proc/meminfo").read_text().splitlines()
                         if line.startswith(("MemAvailable:", "MemFree:"))}
    # This is a TCP connection count, explicitly not a concurrent-request metric.
    try:
        connections = command(["ss", "-Hnt", "state", "established",
                               f"( sport = :{GATEWAY_PORT} )"], check=False,
                              capture_output=True, timeout=15)
        record["gateway_established_connections"] = (
            len(connections.stdout.splitlines()) if connections.returncode == 0 else None)
    except subprocess.TimeoutExpired:
        record["gateway_established_connections"] = None
        record["connections_error"] = "Socket lookup timed out; connection count unknown"
    stream.write(json.dumps(record) + "\n")
    stream.flush()
    available_kb = int(record["meminfo"]["MemAvailable"].split()[0])
    if available_kb < 64 * 1024 * 1024:
        raise RuntimeError("Safety stop: host available memory below 64GiB")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--script", type=Path, required=True)
    parser.add_argument("--k6-image", default="k6-customer:latest")
    parser.add_argument("--run-id", type=int, default=3)
    parser.add_argument("--shards", type=int, default=16)
    parser.add_argument("--probe-only", action="store_true",
                        help="Run direct headroom and a short 15k gateway diagnostic, not formal comparison")
    args = parser.parse_args()
    if args.run_id < 1 or args.shards not in (1, 4, 8, 16):
        raise ValueError("run-id must be positive; shards must be 1, 4, 8 or 16")
    global PREFIX, FARM, GATEWAY, K6
    PREFIX = f"xlite-single-1013-r{args.run_id}"
    FARM, GATEWAY, K6 = PREFIX + "-farm", PREFIX + "-gateway", PREFIX + "-k6"
    k6_names = [f"{K6}-{i}" for i in range(args.shards)]
    root = args.root.resolve()
    script = args.script.resolve()
    results = root / "results"
    results.mkdir(exist_ok=True)
    if (results / "env.json").exists():
        raise RuntimeError("Existing run detected; refusing to overwrite results")
    for port in [*PORTS, GATEWAY_PORT]:
        with socket.socket() as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                sock.bind(("127.0.0.1", port))
            except OSError as exc:
                raise RuntimeError(f"Task port {port} is occupied; not touching its owner") from exc
    for name in [FARM, GATEWAY, *k6_names]:
        exists = command(["docker", "inspect", name], check=False, capture_output=True)
        if exists.returncode == 0:
            raise RuntimeError(f"Existing container {name}; refusing to reuse it")

    image = PREFIX + ":ad4e653"
    (root / ".dockerignore").write_text(
        "source\nresults\n*.env\ncatalog.yaml\nsingle_1013.js\nsingle_1013.py\n")
    dockerfile = root / "Dockerfile.runtime"
    dockerfile.write_text("FROM debian:bookworm-slim\nCOPY bin/xlite bin/mock-llm /usr/local/bin/\n"
                          "COPY farm.sh /farm.sh\n")
    farm_script = root / "farm.sh"
    farm_script.write_text("#!/bin/sh\nset -eu\n"
                           "for p in $(seq 43101 43124); do\n"
                           "  PORT=$p TOKIO_WORKER_THREADS=2 MOCK_TTFT=500 MOCK_OUT_TOKENS=500 "
                           "MOCK_TOKEN_INTERVAL=10 MOCK_BATCH=1 /usr/local/bin/mock-llm &\n"
                           "done\nwait\n")
    with (results / "image-build.log").open("w") as log:
        command(["docker", "build", "--pull=false", "--network=none", "-f", str(dockerfile),
                 "-t", image, str(root)], stdout=log, stderr=subprocess.STDOUT)
    k6_image = capture(["docker", "image", "inspect", "--format", "{{.Id}}", args.k6_image])
    metadata = {"commit": "ad4e65342d00f07f2ec5cc93224089e28084b44a",
                "topology": f"single xlite + 24 Rust mocks + {args.shards} k6 shards, all loopback",
                "xlite_sha256": sha(root / "bin/xlite"),
                "mock_sha256": sha(root / "bin/mock-llm"), "script_sha256": sha(script),
                "driver_sha256": sha(Path(__file__).resolve()),
                "k6_image_id": k6_image, "runtime_image_id": capture(
                    ["docker", "image", "inspect", "--format", "{{.Id}}", image]),
                "levels": [1000, 2000, 5000, 10000, 15000, 18000],
                "warmup_s": 30, "duration_s": 300, "graceful_stop_s": 600,
                "mock": {"ttft": 500, "token_interval": 10, "out_tokens": 500, "batch": 1},
                "cpus": {"gateway": "0-15,64-79", "k6": "16-47,80-111", "farm": "48-63,112-127"},
                "cpu_quotas": {"gateway": 28, "k6_total": 56, "farm": 28},
                "k6_shards": args.shards,
                "gateway_workers": 32, "mock_workers_per_process": 2,
                "k6_gogc": 400,
                "probe_only": args.probe_only,
                "telemetry": "cgroup v2 CPU counters/memory.current and host ss; unknown on lookup errors",
                "h2c": True, "max_conns_per_host": 32, "stream_cap": "peer decides",
                "nofile": 1048576, "hostname": socket.gethostname(), "start_epoch": time.time()}
    metadata["k6_version"] = capture(["docker", "run", "--rm", "--network", "none", k6_image, "version"])
    (results / "env.json").write_text(json.dumps(metadata, indent=2) + "\n")
    created = []
    active_k6 = False
    try:
        farm_args = ["docker", "run", "-d", "--name", FARM, "--memory", "64g",
                     "--cpuset-cpus", metadata["cpus"]["farm"], "--cpus", "28",
                     "--ulimit", "nofile=1048576:1048576"]
        for port in PORTS:
            farm_args += ["-p", f"127.0.0.1:{port}:{port}"]
        capture([*farm_args, image, "sh", "/farm.sh"])
        created.append(FARM)
        for attempt in range(30):
            try:
                for port in PORTS:
                    request(f"http://127.0.0.1:{port}/health")
                break
            except (OSError, urllib.error.URLError):
                time.sleep(1)
        else:
            raise RuntimeError("Mock health checks failed")
        configs = []
        for port in PORTS:
            _, raw = request(f"http://127.0.0.1:{port}/_config", {})
            config = json.loads(raw)
            for key, expected in metadata["mock"].items():
                if config.get(key) != expected:
                    raise RuntimeError(f"Mock config mismatch on {port}: {key}")
            configs.append({"port": port, "config": config})
        (results / "mock-configs.json").write_text(json.dumps(configs, indent=2) + "\n")

        # Import into this run's new SQLite file. Request handling uses a frozen snapshot.
        # Test credentials stay out of the result directory and stdout.
        env_file = root / "gateway.env"
        with env_file.open("x") as stream:
            os.chmod(env_file, 0o600)
            stream.write(f"DATABASE_URL=sqlite:/run/catalog.db\nLISTEN_HOST=127.0.0.1\n"
                         f"PORT={GATEWAY_PORT}\nXLITE_UPSTREAM_H2C=1\n"
                         "XLITE_UPSTREAM_MAX_CONNS_PER_HOST=32\nXLITE_CATALOG_RELOAD_SECS=0\n"
                         "TOKIO_WORKER_THREADS=32\nRUST_LOG=warn\n"
                         f"ADMIN_PASSWORD={secrets.token_urlsafe(32)}\n"
                         f"ADMIN_SESSION_SECRET={secrets.token_urlsafe(32)}\n")
        source_yaml = (root / "source/examples/mock-24.yaml").read_text()
        for old, new in zip(range(4101, 4125), PORTS):
            source_yaml = source_yaml.replace(str(old), str(new))
        source_yaml = source_yaml.replace("  - name: mock\n", "  - name: mock-model\n")
        catalog = root / "catalog.yaml"
        catalog.write_text(source_yaml)
        import_args = ["docker", "run", "--rm", "--network", "host", "--env-file", str(env_file),
                       "-v", f"{root}:/run", image, "xlite", "import", "/run/catalog.yaml"]
        with (results / "catalog-import.log").open("w") as log:
            command(import_args, stdout=log, stderr=subprocess.STDOUT)
        capture(["docker", "run", "-d", "--name", GATEWAY, "--network", "host",
                 "--cpuset-cpus", metadata["cpus"]["gateway"], "--cpus", "28", "--memory", "64g",
                 "--ulimit", "nofile=1048576:1048576", "--env-file", str(env_file),
                 "-v", f"{root}:/run", image, "xlite"])
        created.append(GATEWAY)
        target = f"http://127.0.0.1:{GATEWAY_PORT}/v1/chat/completions"
        for attempt in range(30):
            try:
                request(f"http://127.0.0.1:{GATEWAY_PORT}/health")
                break
            except (OSError, urllib.error.URLError):
                time.sleep(1)
        else:
            raise RuntimeError("Gateway health check failed")
        smoke_data = {"direct": smoke(f"http://127.0.0.1:{PORTS[0]}/v1/chat/completions"),
                      "gateway": smoke(target)}
        if smoke_data["direct"]["bytes"] != smoke_data["gateway"]["bytes"]:
            raise RuntimeError("Direct/gateway payload sizes differ")
        (results / "smoke.json").write_text(json.dumps(smoke_data, indent=2) + "\n")
        print("SMOKE PASS " + json.dumps(smoke_data), flush=True)

        direct_urls = ",".join(f"http://127.0.0.1:{p}/v1/chat/completions" for p in PORTS)
        # Preflight farm headroom, then full diagnostic and historical-sampling tracks.
        runs = [("direct", 18000, 1.0, 128, 60)]
        if args.probe_only:
            runs += [("probe", 15000, 1.0, 128, 60)]
        else:
            runs += [("full", n, 1.0, 128, 300) for n in metadata["levels"]]
            runs += [("half", n, 0.5, 64, 300) for n in [1000, 2000, 5000, 10000]]
        rows = []
        for track, n, sample, chunk_max, seconds in runs:
            label = f"{track}-B-{n}"
            print(f"START {label} sample={sample} chunk_max={chunk_max} shards={args.shards} "
                  f"epoch={time.time()}", flush=True)
            start = time.monotonic()
            previous_resources = {}
            processes, logs, summary_paths = [], [], []
            try:
                for index, name in enumerate(k6_names):
                    shard_vus = n // args.shards + (1 if index < n % args.shards else 0)
                    summary_path = results / f"{label}.shard-{index}.summary.json"
                    summary_paths.append(summary_path)
                    log = (results / f"{label}.shard-{index}.log").open("w")
                    logs.append(log)
                    run_args = ["docker", "run", "--rm", "--name", name, "--network", "host",
                            "--user", f"{os.getuid()}:{os.getgid()}", "--cpuset-cpus",
                            metadata["cpus"]["k6"], "--cpus", str(56 / args.shards),
                            "--memory", f"{192 // args.shards}g",
                            "--ulimit", "nofile=1048576:1048576",
                            "-v", f"{script.parent}:/scripts:ro", "-v", f"{results}:/results",
                            "-e", f"GOMAXPROCS={max(2, round(56 / args.shards))}",
                            "-e", "GOGC=400",
                            k6_image, "run", "--quiet", "--compatibility-mode=base",
                            "-e", f"VUS={shard_vus}", "-e", "WARMUP=30",
                            "-e", f"DURATION_SECONDS={seconds}", "-e", f"KEEP_SAMPLE={sample}",
                            "-e", f"CHUNK_MAX={chunk_max}", "-e", "TOKEN=sk-loadtest",
                            "-e", f"TARGET_URLS={direct_urls if track == 'direct' else target}",
                            "--summary-export", f"/results/{summary_path.name}",
                            f"/scripts/{script.name}"]
                    processes.append(subprocess.Popen(run_args, stdout=log, stderr=subprocess.STDOUT))
                    active_k6 = True
                with (results / f"{label}.resources.jsonl").open("w") as resource:
                    while any(proc.poll() is None for proc in processes):
                        time.sleep(5)
                        monitor([FARM, GATEWAY, *k6_names], resource, time.monotonic() - start,
                                previous_resources)
                        if time.monotonic() - start > 30 + seconds + 630:
                            raise RuntimeError("Safety stop: k6 exceeded scenario deadline")
                return_codes = [proc.wait() for proc in processes]
            finally:
                for name, proc in zip(k6_names, processes):
                    if proc.poll() is None:
                        command(["docker", "stop", "--time", "10", name], check=False, capture_output=True)
                        proc.wait(timeout=30)
                for log in logs:
                    log.close()
                active_k6 = False
            elapsed = time.monotonic() - start
            shard_rows = []
            for index, (summary_path, rc) in enumerate(zip(summary_paths, return_codes)):
                if not summary_path.exists():
                    raise RuntimeError(f"{label}: shard {index} missing summary, exit={rc}")
                shard_vus = n // args.shards + (1 if index < n % args.shards else 0)
                row = classify(json.loads(summary_path.read_text()), shard_vus, sample, elapsed, seconds)
                row.update(shard=index, vus=shard_vus, exit_code=rc)
                if rc != 0:
                    row["valid"] = False
                    row["invalid_reasons"].append(f"shard {index}: k6 exit {rc}")
                shard_rows.append(row)
            row = aggregate(shard_rows)
            row.update(track=track, vus=n, sample=sample, chunk_max=chunk_max, elapsed_s=elapsed,
                       label=label)
            if track == "direct" and (row["estimated_active"] is None or
                                      row["estimated_active"] < n * 0.85 or
                                      row["http_duration_ms"].get("avg", float("inf")) > 7000):
                row["valid"] = False
                row["invalid_reasons"].append("Mock/client capacity preflight lacks headroom")
            rows.append(row)
            (results / "progress.json").write_text(json.dumps(rows, indent=2) + "\n")
            print("END " + json.dumps(row), flush=True)
            if not row["valid"]:
                raise RuntimeError(f"{label}: invalid run; not advancing")
            time.sleep(10)
        (results / "DONE.json").write_text(json.dumps({"end_epoch": time.time(), "runs": len(rows)}) + "\n")
    finally:
        if active_k6:
            for name in k6_names:
                command(["docker", "stop", "--time", "10", name], check=False, capture_output=True)
        for name in reversed(created):
            with (results / f"{name}.log").open("w") as log:
                command(["docker", "logs", name], check=False, stdout=log, stderr=subprocess.STDOUT)
            command(["docker", "stop", "--time", "10", name], check=False, capture_output=True)
        # Keep stopped task containers as evidence; never remove unrelated containers.


if __name__ == "__main__":
    main()
