#!/usr/bin/env python3
"""Verify downloaded raw summaries and export byte-faithful flat stage logs."""
import argparse
import csv
import hashlib
import json
import math
from pathlib import Path

PROFILES = {"A": (100, 300), "B": (500, 300), "C": (1200, 640),
            "D": (6000, 640), "E": (250, 300), "F": (3000, 640)}
LEVELS = [1000, 2000, 5000, 10000, 15000, 18000]
INTERVALS = {"A": 50, "B": 10, "C": 50, "D": 10, "E": 20, "F": 20}


def values(summary, name):
    item = summary["metrics"].get(name, {})
    return item.get("values", item)


def counter(summary, name):
    return values(summary, name).get("count", 0)


def close(actual, expected, label):
    if actual is None and expected is None:
        return
    if expected is None:
        raise RuntimeError(f"Unknown raw value presented as measured: {label}")
    if actual is None or not math.isclose(actual, expected, rel_tol=1e-8, abs_tol=1e-6):
        raise RuntimeError(f"Recomputation mismatch: {label}")


def verify(results, row):
    group, track, gateway, vus = row["group"], row["track"], row["gateway"], row["vus"]
    if group not in PROFILES or row.get("sample") != 0.5:
        raise RuntimeError("Unknown profile or changed sampling contract")
    seconds = PROFILES[group][1]
    if track == "formal":
        if gateway not in ["xlite", "newapi"] or vus not in LEVELS or row["duration_s"] != seconds:
            raise RuntimeError("Frozen formal stage dimensions changed")
    elif track == "preflight":
        if gateway != "direct" or vus != 18000 or row["duration_s"] != (60 if seconds == 300 else 180):
            raise RuntimeError("Frozen preflight dimensions changed")
    else:
        raise RuntimeError("Unknown track")
    if row["label"] != f"{track}-{gateway}-{group}-{vus}" or len(row["shards"]) != 16:
        raise RuntimeError("Stage label/shard cardinality mismatch")
    for index, shard in enumerate(row["shards"]):
        if shard["shard"] != index or shard["vus"] != vus // 16 + int(index < vus % 16):
            raise RuntimeError("Prescribed VU partition mismatch")
    summaries = []
    derived_validity = []
    for index in range(16):
        summary = json.loads((results / f"{row['label']}.shard-{index}.summary.json").read_text())
        shard = row["shards"][index]
        for source, target in [("http_reqs", "requests"), ("body_samples", "body_samples"),
                               ("chunk_samples_added", "chunk_samples")]:
            close(shard[target], counter(summary, source), row["label"] + ":" + source)
        checks = values(summary, "checks")
        total_checks = checks.get("passes", 0) + checks.get("fails", 0)
        failure = values(summary, "http_req_failed")
        valid = shard["requests"] > 0 and shard["exit_code"] == 0 and \
            total_checks == shard["requests"] + shard["body_samples"] and \
            row.get("elapsed_s", 0) >= 30 + row["duration_s"] - 1 and \
            failure.get("value", failure.get("rate")) == 0 and not checks.get("fails", 0)
        valid = valid and not any(counter(summary, name)
                                  for name in ["body_errors", "transport_errors", "non200_responses"])
        if shard["requests"]:
            tolerance = max(0.02, 5 * math.sqrt(0.25 / shard["requests"]))
            valid = valid and abs(shard["body_samples"] / shard["requests"] - 0.5) <= tolerance
        expected = shard["body_samples"] * min(64, PROFILES[row["group"]][0] - 1) \
            if row["timestamp_diagnostics"] else 0
        valid = valid and shard["chunk_samples"] == expected and \
            counter(summary, "timestamp_diagnostic_bodies") == \
            (shard["body_samples"] if row["timestamp_diagnostics"] else 0)
        if bool(shard["valid"]) != bool(valid):
            raise RuntimeError("Declared shard validity differs from raw validity gates")
        derived_validity.append(bool(valid))
        summaries.append(summary)
    for name in ["requests", "body_samples", "chunk_samples"]:
        close(row[name], sum(shard[name] for shard in row["shards"]), name)
    rates = [values(s, "http_reqs").get("rate") for s in summaries]
    close(row["qps_whole_run"], sum(rates) if all(n is not None for n in rates) else None, "whole QPS")
    close(row["steady_success_qps"],
          sum(counter(s, "steady_successful_requests") for s in summaries) / row["duration_s"],
          "steady QPS")
    active_parts = []
    for shard, summary in zip(row["shards"], summaries):
        http_avg = values(summary, "http_req_duration").get("avg")
        iteration_avg = values(summary, "iteration_duration").get("avg")
        active_parts.append(shard["vus"] * http_avg / iteration_avg
                            if http_avg is not None and iteration_avg else None)
    active = sum(active_parts) if all(n is not None for n in active_parts) else None
    close(row["estimated_active"], active, "activity")
    for source, target in [("ttft_ms", "ttft_ms"), ("http_req_duration", "http_duration_ms"),
                           ("iteration_duration", "iteration_duration_ms")]:
        averages = [values(s, source).get("avg") for s in summaries]
        average = sum(n * counter(s, "http_reqs") for n, s in zip(averages, summaries)) / row["requests"] \
            if row["requests"] and all(n is not None for n in averages) else None
        close(row[target].get("avg"), average, source + " weighted mean")
        for percentile in ["p(50)", "p(90)", "p(95)", "p(99)"]:
            numbers = [values(s, source).get(percentile) for s in summaries]
            actual = row[target].get(percentile + "_shard_range")
            if all(n is not None for n in numbers):
                if actual is None:
                    raise RuntimeError("Measured raw percentile missing from aggregate")
                close(actual[0], min(numbers), percentile + " low")
                close(actual[1], max(numbers), percentile + " high")
            elif actual is not None:
                raise RuntimeError("Unknown percentile presented as measured")
    valid = all(derived_validity)
    if row["gateway"] == "direct":
        ideal = 500 + PROFILES[row["group"]][0] * INTERVALS[row["group"]]
        valid = valid and active is not None and active >= row["vus"] * 0.85 and \
            row["http_duration_ms"].get("avg", float("inf")) <= ideal * 1.28
    if bool(row["valid"]) != bool(valid):
        raise RuntimeError("Declared aggregate validity differs from raw gates")
    if not valid and not row.get("invalid_reasons"):
        raise RuntimeError("Invalid stage missing failure reasons")


def span(row, percentile):
    pair = row["ttft_ms"].get(percentile + "_shard_range")
    return f"{pair[0]:.3f}–{pair[1]:.3f}" if pair else "unknown"


def number(value):
    return f"{value:.3f}" if value is not None else "unknown"


def verify_group_coverage(results, rows):
    manifest = json.loads((results / "GROUP_COVERAGE.json").read_text())
    sources = manifest["groups"]
    if set(sources) != set(PROFILES):
        raise RuntimeError("Group coverage does not contain exactly A-F")
    common = None
    configurations = {}
    for group, relative in sources.items():
        source = (results / relative).resolve()
        source_rows = json.loads((source / "progress.json").read_text())
        selected = [row for row in source_rows if row["group"] == group]
        combined = [row for row in rows if row["group"] == group]
        if selected != combined or len(selected) != 7 or not all(row["valid"] for row in selected):
            raise RuntimeError("Group rows differ from original complete curve")
        if not (source / "DONE.json").exists() and not (source / "FAILED-all.json").exists():
            raise RuntimeError("Source execution has no terminal record")
        env = json.loads((source / "matrix-env.json").read_text())
        if env.get("scope") != "xlite only" or env.get("sample") != 0.5 or \
                env.get("cpu_quotas") != {"gateway": 28, "k6": 56, "farm": 28} or \
                env.get("memory_gib") != {"gateway": 64, "k6": 192, "farm": 64} or \
                env.get("cpu_masks") != {"gateway": "0-15,64-79", "k6": "16-47,80-111",
                                         "farm": "48-63,112-127"}:
            raise RuntimeError("Group source changed execution scope or hard budgets")
        if (source / "DONE.json").exists():
            done = json.loads((source / "DONE.json").read_text())
            if done.get("rows") != len(source_rows) or \
                    done.get("formal_stages") != sum(row["track"] == "formal" for row in source_rows):
                raise RuntimeError("Source DONE counts differ from actual rows")
        provenance = env["provenance"]
        code = source.parent
        files = {"driver_sha256": "xlite_af_1013.py", "shared_driver_sha256": "sonic_af_1013.py",
                 "helper_sha256": "single_1013.py", "script_sha256": "sonic_af_1013.js",
                 "catalog_source_sha256": "mock-24.yaml"}
        for key, name in files.items():
            if hashlib.sha256((code / name).read_bytes()).hexdigest() != provenance[key]:
                raise RuntimeError("Source execution code fingerprint mismatch")
        for name, expected in provenance["binary_sha256"].items():
            if hashlib.sha256((code / "bin" / name).read_bytes()).hexdigest() != expected:
                raise RuntimeError("Source binary fingerprint mismatch")
        identity = {key: provenance[key] for key in ["image", "k6_image", "script_sha256",
                                                     "helper_sha256", "binary_sha256"]}
        if common is not None and common != identity:
            raise RuntimeError("Group batches changed gateway/mock/client identity")
        common = identity
        configurations[group] = {"source": relative, "k6_memory_limit": env.get("k6_memory_limit"),
                                 "cpu_quotas": env["cpu_quotas"], "memory_gib": env["memory_gib"]}
        for row in selected:
            verify(source, row)
            for index in range(16):
                for suffix in ["summary.json", "log"]:
                    name = f"{row['label']}.shard-{index}.{suffix}"
                    if (source / name).read_bytes() != (results / name).read_bytes():
                        raise RuntimeError("Export shard bytes differ from executed source")
    limits = {group: value["k6_memory_limit"] for group, value in configurations.items()}
    if limits != {group: "9GiB" if group in ["D", "F"] else None for group in PROFILES}:
        raise RuntimeError("Unexpected group-specific client GC configuration")
    return configurations


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--output-base", type=Path, required=True)
    parser.add_argument("--partial", action="store_true", help="Explicit incomplete/failed archive")
    parser.add_argument("--xlite-only", action="store_true", help="Independent 36-stage xlite matrix")
    parser.add_argument("--group-coverage", action="store_true",
                        help="Verified whole-group curves from documented terminal batches, not one DONE")
    args = parser.parse_args()
    results = args.results.resolve()
    rows = json.loads((results / "progress.json").read_text())
    formal = [row for row in rows if row["track"] == "formal"]
    preflights = [row for row in rows if row["track"] == "preflight"]
    gateways = ["xlite"] if args.xlite_only else ["xlite", "newapi"]
    expected = {(gateway, group, vus) for gateway in gateways
                for group in PROFILES for vus in LEVELS}
    actual = {(row["gateway"], row["group"], row["vus"]) for row in formal}
    for row in rows:
        verify(results, row)
    configurations = None
    if args.group_coverage:
        if not args.xlite_only:
            raise RuntimeError("Group coverage is only supported for xlite")
        configurations = verify_group_coverage(results, rows)
    done_path = results / "DONE.json"
    done = json.loads(done_path.read_text()) if done_path.exists() else None
    preflight_keys = {(row["gateway"], row["group"], row["vus"]) for row in preflights}
    count = 36 * len(gateways)
    complete = done is not None and done.get("rows") == count + 6 and done.get("formal_stages") == count and \
        actual == expected and len(formal) == count and len(rows) == count + 6 and len(preflights) == 6 and \
        preflight_keys == {("direct", group, 18000) for group in PROFILES} and \
        all(row["valid"] for row in rows)
    if configurations is not None:
        complete = actual == expected and len(formal) == 36 and len(rows) == 42 and len(preflights) == 6 and \
            preflight_keys == {("direct", group, 18000) for group in PROFILES} and all(row["valid"] for row in rows)
    if not complete and not args.partial:
        raise RuntimeError("Incomplete/failed matrix; use --partial, never fabricate complete results")
    base = args.output_base.resolve()
    details = base / ("retest-single-xlite-af50-details" if args.xlite_only else "retest-single-sonic-af50-details")
    destinations = {gateway: base / ("retest-single-xlite-af50" if gateway == "xlite"
                                    else "retest-single-newapi-sonic-af50") for gateway in gateways}
    if any(path.exists() for path in [details, *destinations.values()]):
        raise RuntimeError("Output exists; refusing to overwrite user/archive data")
    details.mkdir()
    for destination in destinations.values():
        destination.mkdir()
    status = ("COMPLETE A-F GROUP COVERAGE / TWO DOCUMENTED EXECUTION BATCHES"
              if complete and configurations is not None else
              "COMPLETE" if complete else "PARTIAL / NOT A COMPLETE MATRIX")
    table = ["# xlite A–F" if args.xlite_only else "# Matched Sonic / xlite A–F", "", status,
             "", "batch=1, TTFT500ms, sample=0.5. TTFT columns are 16-shard ranges, not global percentiles.",
             "Pinned xlite ad4e653 / 0.5.3; not a measurement of latest xlite.",
             "Private internal Docker bridge; xlite upstream h2c, private SQLite frozen catalog.",
             "New API was not tested in this independent batch." if args.xlite_only else
             "Supplied Sonic v0.0.0; native transport/SQLite, CountToken effect unverified.",
             "", "| Gateway | Group | VU | Valid | Whole QPS | Steady success QPS | Activity | TTFT P50 range ms | P99 range ms |",
             "|---|---|---:|---|---:|---:|---:|---:|---:|"]
    if configurations is not None:
        table[3:3] = ["", "Group configurations: `" + json.dumps(configurations, sort_keys=True) + "`",
                      "A/B/C/E: default Go soft limit. D/F: GOMEMLIMIT=9GiB; all hard budgets unchanged.",
                      "r10 D direct preflight hit client OOM. No within-group load curve mixes settings."]
    records = []
    for gateway, destination in destinations.items():
        stage_rows = sorted((row for row in formal if row["gateway"] == gateway),
                            key=lambda row: (row["group"], row["vus"]))
        progress = [status, "16 original shards per stage. Percentiles are ranges, not global quantiles."]
        for row in stage_rows:
            _, seconds = PROFILES[row["group"]]
            name = f"{row['group']}_{row['vus']}_{seconds}s.log"
            header = (f"{status}\nGateway={gateway}, sample=0.5, batch=1, stage={row['label']}, "
                      f"valid={row['valid']}\n"
                      f"Whole QPS={number(row['qps_whole_run'])}; steady success QPS={number(row['steady_success_qps'])}; "
                      f"estimated activity={number(row['estimated_active'])}/{row['vus']}\n"
                      f"Failure reasons={row.get('invalid_reasons', [])}\n"
                      f"Group execution config={configurations[row['group']] if configurations else 'single batch'}\n"
                      f"TTFT P50 shard range={span(row, 'p(50)')}ms; "
                      f"P99 shard range={span(row, 'p(99)')}ms (NOT global quantiles)\n")
            with (destination / name).open("xb") as output:
                output.write(header.encode())
                for index in range(16):
                    output.write(f"\n===== ORIGINAL SHARD {index} =====\n".encode())
                    output.write((results / f"{row['label']}.shard-{index}.log").read_bytes())
            progress.append(header.strip())
            table.append(f"| {gateway} | {row['group']} | {row['vus']} | {row['valid']} | "
                         f"{number(row['qps_whole_run'])} | {number(row['steady_success_qps'])} | "
                         f"{number(row['estimated_active'])} | {span(row, 'p(50)')} | {span(row, 'p(99)')} |")
            records.append({"gateway": gateway, "group": row["group"], "vus": row["vus"],
                            "valid": row["valid"], "whole_qps": row["qps_whole_run"],
                            "steady_success_qps": row["steady_success_qps"],
                            "estimated_active": row["estimated_active"],
                            "ttft_p50_shard_low_ms": (row["ttft_ms"].get("p(50)_shard_range") or [None, None])[0],
                            "ttft_p50_shard_high_ms": (row["ttft_ms"].get("p(50)_shard_range") or [None, None])[1],
                            "ttft_p99_shard_low_ms": (row["ttft_ms"].get("p(99)_shard_range") or [None, None])[0],
                            "ttft_p99_shard_high_ms": (row["ttft_ms"].get("p(99)_shard_range") or [None, None])[1]})
        (destination / "all_progress.log").write_text("\n\n".join(progress) + "\n")
    (details / "comparison.md").write_text("\n".join(table) + "\n")
    (details / "verified-progress.json").write_text(json.dumps(rows, indent=2) + "\n")
    if records:
        with (details / "comparison.csv").open("w", newline="") as output:
            writer = csv.DictWriter(output, fieldnames=list(records[0]))
            writer.writeheader(); writer.writerows(records)
    checksums = []
    for directory in [details, *destinations.values()]:
        for path in sorted(directory.rglob("*")):
            if path.is_file():
                checksums.append(hashlib.sha256(path.read_bytes()).hexdigest() + "  " +
                                 str(path.relative_to(base)))
    (details / "SHA256SUMS").write_text("\n".join(checksums) + "\n")
    print(f"Verified {len(rows)} rows / {len(rows) * 16} summaries; status={status}")


if __name__ == "__main__":
    main()
