#!/usr/bin/env python3
"""把一轮定档位压测的结果目录汇总成报告，并在链路超预算时**拒绝出报告**。

输入 = `scripts/loadtest/run_levels.sh` 写出的结果目录：

    <RESULT_DIR>/env.txt                 跑数元数据（KEY=VALUE，见 §报告契约）
    <RESULT_DIR>/<LEVEL>.log             k6 原始日志（指纹：scenario 行的 gracefulStop）
    <RESULT_DIR>/<LEVEL>.summary.json    k6 --summary-export

输出 = 同目录下 `meta.json` + stdout 的 markdown 表。

它做三件事（契约见 `docs/manual/v88-rig.md` §1.3）：

1. **指纹校验**：每档 `checks_total / http_reqs` 必须 ≈ 2.00（`KEEP_BODY=true` 的全量
   解析口径），且日志头必须是 `gracefulStop: 10m0s`。不满足的档标 `INVALID`，不进表 ——
   客户机那次就是靠这两个数发现「挂载的 k6.js 被换过」的（`v88-rig.md` §1.1）。
2. **活跃流**：`活跃流 = qps × dur_p50`（`v88-rig.md` §2 的稳态近似）。
3. **链路预算**：用 `link_budget.py` 判每档是否 LINK-BOUND；有任一档越界就退出码 2、
   **不出报告**（因为那量的是网线，不是网关）。

退出码：0 = 报告可用；2 = 有档 LINK-BOUND（拒绝出报告）；3 = 有档指纹不符或没有有效档位。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from link_budget import (  # noqa: E402
    DEFAULT_BYTES_PER_CHUNK,
    DEFAULT_EFFICIENCY,
    read_link_mbps,
    usable_mbps,
    verdict,
)

EXPECT_CHECKS_RATIO = 2.0
EXPECT_GRACEFUL_STOP = "10m0s"
RATIO_TOLERANCE = 0.01
LEVEL_RE = re.compile(r"^(\d+)")


def read_env_txt(result_dir: str) -> dict[str, str]:
    """读 run_levels.sh 写的 env.txt（不存在时返回空）。"""
    env: dict[str, str] = {}
    path = os.path.join(result_dir, "env.txt")
    if not os.path.isfile(path):
        return env
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            env[key.strip()] = value.strip()
    return env


def graceful_stop_of(log_path: str) -> str:
    """从 k6 日志头取 scenario 的 gracefulStop（取不到返回 ''）。"""
    if not os.path.isfile(log_path):
        return ""
    with open(log_path, encoding="utf-8", errors="replace") as handle:
        for line in handle:
            match = re.search(r"gracefulStop: ([0-9a-z]+)", line)
            if match:
                return match.group(1)
    return ""


def load_level(result_dir: str, stem: str) -> dict:
    """解析一档的 summary + 日志指纹。"""
    with open(os.path.join(result_dir, f"{stem}.summary.json"), encoding="utf-8") as handle:
        summary = json.load(handle)
    metrics = summary.get("metrics", {})

    def metric(name: str) -> dict:
        value = metrics.get(name)
        return value if isinstance(value, dict) else {}

    reqs = metric("http_reqs").get("count", 0)
    checks = metric("checks")
    passes, fails = checks.get("passes", 0), checks.get("fails", 0)
    total_checks = passes + fails
    ratio = (total_checks / reqs) if reqs else 0.0
    dur_ms = metric("http_req_duration").get("med")
    ttft_ms = metric("ttft_ms").get("med")
    data_bytes = metric("data_received").get("count", 0)
    graceful = graceful_stop_of(os.path.join(result_dir, f"{stem}.log"))

    vus_match = LEVEL_RE.match(stem)
    return {
        "level": stem,
        "vus": int(vus_match.group(1)) if vus_match else None,
        "http_reqs": reqs,
        "qps": metric("http_reqs").get("rate", 0.0),
        "checks_ratio": ratio,
        "dur_ms": dur_ms,
        "ttft_ms": ttft_ms,
        "bytes_per_stream": (data_bytes / reqs) if reqs else 0.0,
        "graceful_stop": graceful,
        "fingerprint_ok": abs(ratio - EXPECT_CHECKS_RATIO) <= RATIO_TOLERANCE
        and graceful == EXPECT_GRACEFUL_STOP,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="汇总定档位压测结果 + 链路预算闸门")
    parser.add_argument("--result-dir", required=True, help="run_levels.sh 的结果目录")
    parser.add_argument("--chunks", type=int, default=500, help="每条流的块数（预算用）")
    parser.add_argument("--bytes-per-chunk", type=int, default=DEFAULT_BYTES_PER_CHUNK,
                        help=f"每块字节数；仅当 summary 里没有 data_received 时使用（默认 {DEFAULT_BYTES_PER_CHUNK}）")
    parser.add_argument("--link-mbps", type=float, help="链路协商速率（Mbps，每方向）")
    parser.add_argument("--link-iface", help="本机网卡名（替代 --link-mbps）")
    parser.add_argument("--efficiency", type=float, default=DEFAULT_EFFICIENCY)
    parser.add_argument("--allow-linkbound", action="store_true", help="越界也出报告（仅用于诊断）")
    parser.add_argument("--allow-fingerprint-mismatch", action="store_true",
                        help="指纹不符也出报告（默认拒绝）")
    args = parser.parse_args(argv)

    stems = sorted(
        (name[: -len(".summary.json")] for name in os.listdir(args.result_dir) if name.endswith(".summary.json")),
        key=lambda stem: int(LEVEL_RE.match(stem).group(1)) if LEVEL_RE.match(stem) else 0,
    )
    if not stems:
        print(f"✗ {args.result_dir} 里没有 *.summary.json", file=sys.stderr)
        return 3

    link_mbps = args.link_mbps
    if link_mbps is None and args.link_iface:
        link_mbps = read_link_mbps(args.link_iface)
    usable = usable_mbps(link_mbps, args.efficiency) if link_mbps else None

    levels = []
    for stem in stems:
        row = load_level(args.result_dir, stem)
        if usable:
            per_stream = row["bytes_per_stream"] or (args.chunks * args.bytes_per_chunk)
            target_secs = (row["dur_ms"] or 0) / 1000
            required = (row["qps"] * target_secs) * per_stream * 8 / target_secs / 1e6 if target_secs else float("inf")
            row["active_streams"] = row["qps"] * target_secs
            row["required_mbps"] = required
            bound, text = verdict(required, usable)
            row["link_bound"], row["verdict"] = bound, text
        else:
            row["active_streams"] = row["qps"] * ((row["dur_ms"] or 0) / 1000)
            row["required_mbps"] = None
            row["link_bound"], row["verdict"] = None, "未提供链路速率"
        levels.append(row)

    env = read_env_txt(args.result_dir)
    meta = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "result_dir": os.path.abspath(args.result_dir),
        "env": env,
        "contract": {
            "checks_ratio": EXPECT_CHECKS_RATIO,
            "tolerance": RATIO_TOLERANCE,
            "graceful_stop": EXPECT_GRACEFUL_STOP,
            "link_mbps": link_mbps,
            "efficiency": args.efficiency,
            "chunks": args.chunks,
        },
        "levels": levels,
    }
    with open(os.path.join(args.result_dir, "meta.json"), "w", encoding="utf-8") as handle:
        json.dump(meta, handle, ensure_ascii=False, indent=2)

    print(f"# {os.path.basename(os.path.abspath(args.result_dir))}")
    if env:
        print("")
        for key in ("GATEWAY_IMAGE", "K6_IMAGE", "K6_SCRIPT", "K6_SCRIPT_MD5", "TOKIO_WORKER_THREADS", "TARGET", "LEVELS"):
            if key in env:
                print(f"- {key}: `{env[key]}`")
    print("")
    print("| 档位 | qps | dur p50 | TTFT p50 | 活跃流 | 链路需求 | 判读 | 指纹 |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | --- | --- |")
    for row in levels:
        dur = f"{row['dur_ms'] / 1000:.2f}s" if row["dur_ms"] else "-"
        ttft = f"{row['ttft_ms']:.0f}ms" if row["ttft_ms"] else "-"
        need = f"{row['required_mbps']:,.0f} Mbps" if row["required_mbps"] else "-"
        mark = "✓" if row["fingerprint_ok"] else "✗"
        print(
            f"| {row['vus']} | {row['qps']:.1f} | {dur} | {ttft} | {row['active_streams']:.0f} | "
            f"{need} | {row['verdict']} | {mark} |"
        )
    print(f"\nmeta.json 已写入 {os.path.join(args.result_dir, 'meta.json')}")

    bad = [row for row in levels if not row["fingerprint_ok"]]
    bound = [row for row in levels if row["link_bound"]]
    if bad and not args.allow_fingerprint_mismatch:
        names = ", ".join(row["level"] for row in bad)
        print(f"\n✗ 指纹不符（checks 比 ≠ {EXPECT_CHECKS_RATIO} 或 gracefulStop ≠ {EXPECT_GRACEFUL_STOP}）：{names}", file=sys.stderr)
        print("  → 这批数不是客户对标喂法（脚本被换过？）。换回仓库版 k6.js 重跑；见 v88-rig.md §1.1/§1.3。", file=sys.stderr)
        return 3
    if bound and not args.allow_linkbound:
        print(f"\n✗ 有 {len(bound)} 档 LINK-BOUND：这台台架量的是网线，不是网关 → 拒绝出报告。", file=sys.stderr)
        print("  → 换 10G/25G，或把 k6 与网关/mock 同机（loopback）后重测。", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
