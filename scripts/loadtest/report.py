#!/usr/bin/env python3
"""把一轮定档位压测的结果目录汇总成报告，并在链路超预算时**拒绝出报告**。

输入 = `scripts/loadtest/run_levels.sh` 写出的结果目录：

    <RESULT_DIR>/env.txt                 跑数元数据（KEY=VALUE，见 §报告契约）
    <RESULT_DIR>/<LEVEL>.log             k6 原始日志（指纹：scenario 行的 gracefulStop）
    <RESULT_DIR>/<LEVEL>.summary.json    k6 --summary-export

输出 = 同目录下 `meta.json` + stdout 的 markdown 表。

它做四件事（契约见 `docs/manual/v88-rig.md` §1.3）：

1. **指纹校验**：每档 `checks_total / http_reqs` 必须 ≈ 2.00（`KEEP_BODY=true` 的全量
   解析口径），且日志头必须是 `gracefulStop: 10m0s`。不满足的档标 `INVALID`，不进表 ——
   客户机那次就是靠这两个数发现「挂载的 k6.js 被换过」的（`v88-rig.md` §1.1）。
2. **活跃流**：**闭式** `活跃流 = VUS × dur/iter`（k6 是闭式负载，一个 VU 同时只有一个请求在飞）。
   早先用的 `qps × dur` 会被 ramp-up / gracefulStop 的排空段**稀释**，系统性低报并发
   —— 实测差 42%（4000 VU：闭式 2670 vs 稀释 1557），所以只留作对照列
   `active_streams_diluted`。**链路预算用闭式那个。**
3. **accept 队列**：读 `run_levels.sh` 采样的 `<LEVEL>.acceptq`（`ss` 的 `Recv-Q` 峰值）。
   峰值逼近 `GATEWAY_BACKLOG` 就是**触顶**的直接证据，会单独告警 —— 这一档量到的是
   连接建立排队，不是网关处理能力。注意 `GATEWAY_BACKLOG` 是监听套接字的 `Send-Q`，
   **不是**内核 `net.core.somaxconn`（libc `SOMAXCONN` 会把 backlog 压到 128）。
4. **链路预算**：用 `link_budget.py` 判每档是否 LINK-BOUND；有任一档越界就退出码 2、
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


def read_acceptq(result_dir: str, stem: str) -> int | None:
    """读 run_levels.sh 采样的 accept 队列深度峰值（ss 的 Recv-Q）。

    峰值逼近监听 backlog 就说明**触顶**了 —— 这是 backlog 是否成为瓶颈的直接证据，
    与负载高低、与内核 somaxconn 都无关。没有这个文件（跨机跑）时返回 None。
    """
    path = os.path.join(result_dir, f"{stem}.acceptq")
    if not os.path.isfile(path):
        return None
    try:
        with open(path, encoding="utf-8") as handle:
            return int(handle.read().strip())
    except (OSError, ValueError):
        return None


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

    # 真实并发：k6 是闭式负载，一个 VU 同时只有一个请求在飞，所以
    #   在飞流 = VUS × (dur / iter)
    # 而 `qps × dur` 会被 ramp-up / gracefulStop 的排空段稀释（run 期间的平均速率 ≠ 稳态速率），
    # 低报并发。两个都留，但**链路预算用闭式那个**。见 §11.6 / v88-rig.md §2。
    vus_max = metric("vus_max")
    vus_actual = vus_max.get("max") or vus_max.get("value")
    iter_ms = metric("iteration_duration").get("med")
    iterations = metric("iterations").get("count", 0)
    iter_per_vu = (iterations / vus_actual) if vus_actual else None
    acceptq = read_acceptq(result_dir, stem)

    vus_match = LEVEL_RE.match(stem)
    return {
        "level": stem,
        "vus": int(vus_match.group(1)) if vus_match else None,
        "vus_actual": vus_actual,
        "http_reqs": reqs,
        "qps": metric("http_reqs").get("rate", 0.0),
        "checks_ratio": ratio,
        "dur_ms": dur_ms,
        "ttft_ms": ttft_ms,
        "iter_ms": iter_ms,
        "iterations": iterations,
        "iter_per_vu": iter_per_vu,
        "acceptq_max": acceptq,
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
        target_secs = (row["dur_ms"] or 0) / 1000
        # 闭式（首选）：VUS × dur/iter。退化到稀释式 qps × dur（缺 iteration_duration 时）。
        if row["vus_actual"] and row["iter_ms"]:
            closed = row["vus_actual"] * target_secs / (row["iter_ms"] / 1000)
        else:
            closed = None
        diluted = row["qps"] * target_secs
        row["active_streams"] = closed if closed is not None else diluted
        row["active_streams_diluted"] = diluted
        # 一个 VU 每 iter 秒发一个请求 → 稳态 qps。与实测 qps 的比值就是排空段的稀释程度。
        row["qps_steady"] = (row["vus_actual"] / (row["iter_ms"] / 1000)) if (row["vus_actual"] and row["iter_ms"]) else None

        per_stream = row["bytes_per_stream"] or (args.chunks * args.bytes_per_chunk)
        if usable and target_secs:
            required = row["active_streams"] * per_stream * 8 / target_secs / 1e6
            row["required_mbps"] = required
            bound, text = verdict(required, usable)
            row["link_bound"], row["verdict"] = bound, text
        else:
            row["required_mbps"] = None
            row["link_bound"], row["verdict"] = None, ("未提供链路速率" if not usable else "无 dur")
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
            "active_streams": "VUS × dur/iter（闭式，首选）；active_streams_diluted = qps × dur 仅作对照",
        },
        "levels": levels,
    }
    with open(os.path.join(args.result_dir, "meta.json"), "w", encoding="utf-8") as handle:
        json.dump(meta, handle, ensure_ascii=False, indent=2)

    print(f"# {os.path.basename(os.path.abspath(args.result_dir))}")
    if env:
        print("")
        for key in ("GATEWAY_IMAGE", "K6_IMAGE", "K6_IMAGE_ID", "K6_VERSION", "K6_SCRIPT_MD5",
                    "K6_SCRIPT_SHA256", "TOKIO_WORKER_THREADS", "XLITE_CATALOG_RELOAD_SECS",
                    "GATEWAY_BACKLOG", "GATEWAY_BACKLOG_SOURCE", "SOMAXCONN", "TARGET", "LEVELS"):
            if key in env:
                print(f"- {key}: `{env[key]}`")
    print("")
    print("| 档位 | qps(实测) | qps(稳态) | dur p50 | TTFT p50 | 活跃流 | 稀释式 | acceptQ峰值 | 链路需求 | 判读 | 指纹 |")
    print("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |")
    for row in levels:
        dur = f"{row['dur_ms'] / 1000:.2f}s" if row["dur_ms"] else "-"
        ttft = f"{row['ttft_ms']:.0f}ms" if row["ttft_ms"] else "-"
        need = f"{row['required_mbps']:,.0f} Mbps" if row["required_mbps"] else "-"
        mark = "✓" if row["fingerprint_ok"] else "✗"
        qs = f"{row['qps_steady']:.1f}" if row["qps_steady"] else "-"
        aq = f"{row['acceptq_max']}" if row["acceptq_max"] is not None else "-"
        print(
            f"| {row['vus']} | {row['qps']:.1f} | {qs} | {dur} | {ttft} | {row['active_streams']:.0f} | "
            f"{row['active_streams_diluted']:.0f} | {aq} | {need} | {row['verdict']} | {mark} |"
        )
    print(f"\nmeta.json 已写入 {os.path.join(args.result_dir, 'meta.json')}")

    # backlog 触顶的直接证据：accept 队列峰值逼近监听 backlog。
    backlog_raw = env.get("GATEWAY_BACKLOG", "")
    if backlog_raw.isdigit():
        backlog = int(backlog_raw)
        hot = [row for row in levels if row["acceptq_max"] is not None and backlog and row["acceptq_max"] >= backlog]
        if hot:
            names = ", ".join(f"{row['level']}(峰值{row['acceptq_max']})" for row in hot)
            print(f"\n⚠️ accept 队列触顶（backlog={backlog}）：{names}", file=sys.stderr)
            print("  → 这一档量到了连接建立排队，不是网关处理能力。加大 backlog 或降低建连突发。", file=sys.stderr)

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
