#!/usr/bin/env python3
"""把一轮定档位压测的结果目录汇总成报告，闸门不过时**拒绝出报告**。

输入 = `scripts/loadtest/run_levels.sh` 写出的结果目录：

    <RESULT_DIR>/env.txt                 跑数元数据（KEY=VALUE，见 §报告契约）
    <RESULT_DIR>/<LEVEL>.log             k6 原始日志（指纹：scenario 行的 gracefulStop）
    <RESULT_DIR>/<LEVEL>.summary.json    k6 --summary-export

输出 = stdout 的 markdown 表 +（**只在闸门通过时**）同目录下 `meta.json`。

它做四件事（契约见 `docs/manual/v88-rig.md` §1.3）：

1. **指纹校验**：每档 `checks_total / http_reqs` 必须 ≈ 2.00（`KEEP_BODY=true` 的全量
   解析口径），且日志头必须是期望的 `gracefulStop`（默认 `10m0s`；用
   `--expect-graceful-stop` 覆盖，`run_levels.sh` 会把同一个值透传过来）。不符的档在表里
   标 **`INVALID`**，且**不进对标表、不参与链路预算闸门** —— 客户机那次就是靠这两个数
   发现「挂载的 k6.js 被换过」的（`v88-rig.md` §1.1）。
2. **活跃流**：**闭式** `活跃流 = VUS × dur/iter`（k6 是闭式负载，一个 VU 同时只有一个请求在飞）。
   早先用的 `qps × dur` 会被 ramp-up / gracefulStop 的排空段**稀释**，系统性低报并发
   —— 实测差 42%（4000 VU：闭式 2670 vs 稀释 1557），所以只留作对照列
   `active_streams_diluted`。缺 `iteration_duration` 时退化用它，并在表里标 `下界`。
   **链路预算用闭式那个。**
3. **accept 队列**：读 `run_levels.sh` 采样的 `<LEVEL>.acceptq`（`ss` 的 `Recv-Q` 峰值）。
   峰值逼近 `GATEWAY_BACKLOG` 就是**触顶**的直接证据，会单独告警 —— 这一档量到的是
   连接建立排队，不是网关处理能力。注意 `GATEWAY_BACKLOG` 是监听套接字的 `Send-Q`，
   **不是**内核 `net.core.somaxconn`（libc `SOMAXCONN` 会把 backlog 压到 128）。
   非 Linux（没有 `ss`）或跨机时该文件是 `unknown`/不存在，表里原样显示，**不会记成 0**。
4. **链路预算**：用 `link_budget.py` 判每档是否 LINK-BOUND；任一**有效**档越界就退出码 2、
   **不写 `meta.json`**（因为那量的是网线，不是网关）。

退出码：0 = 报告可用（已写 `meta.json`）；2 = 有档 LINK-BOUND（REFUSED）；3 = 有档指纹不符
（REFUSED）或没有有效档位。`--allow-linkbound` / `--allow-fingerprint-mismatch` 只在诊断时
用来绕过对应闸门（此时会照写 `meta.json`，但报告头会标注是诊断模式）。
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
    DEFAULT_EFFICIENCY,
    read_link_mbps,
    usable_mbps,
    verdict,
)

DEFAULT_EXPECT_CHECKS_RATIO = 2.0
DEFAULT_EXPECT_GRACEFUL_STOP = "10m0s"
RATIO_TOLERANCE = 0.01
LEVEL_RE = re.compile(r"^(\d+)")
# k6 打印的是 `10s` / `1m30s` / `10m0s`，而 GRACEFUL_STOP 常写成 `600s` —— 两者都比成秒。
DURATION_RE = re.compile(r"^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+(?:\.\d+)?)s)?$")


def duration_secs(text: str) -> float | None:
    """把 `600s` / `10m0s` / `1m30s` / `2h` 解析成秒；解析不了返回 None。"""
    text = (text or "").strip()
    match = DURATION_RE.match(text)
    if not match or not any(match.groups()):
        return None
    hours, minutes, secs = (float(g) if g else 0.0 for g in match.groups())
    return hours * 3600 + minutes * 60 + secs


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


def read_acceptq(result_dir: str, stem: str) -> int | str | None:
    """读 run_levels.sh 采样的 accept 队列深度峰值（ss 的 Recv-Q）。

    峰值逼近监听 backlog 就说明**触顶**了 —— 这是 backlog 是否成为瓶颈的直接证据，
    与负载高低、与内核 somaxconn 都无关。返回 int；`unknown`（没有 `ss`，例如 macOS）
    或文件不存在时原样返回字符串/None，**绝不当成 0**。
    """
    path = os.path.join(result_dir, f"{stem}.acceptq")
    if not os.path.isfile(path):
        return None
    try:
        with open(path, encoding="utf-8") as handle:
            raw = handle.read().strip()
    except OSError:
        return None
    return int(raw) if raw.isdigit() else (raw or None)


def load_level(result_dir: str, stem: str, expect_ratio: float, expect_graceful: str) -> dict:
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
    acceptq = read_acceptq(result_dir, stem)

    def same_number(a: float, b: float) -> bool:
        return abs(a - b) <= RATIO_TOLERANCE

    # gracefulStop 比"秒数"而不是字符串：k6 把 600s 打印成 `10m0s`，而人写的是 `600s`。
    got_secs, want_secs = duration_secs(graceful), duration_secs(expect_graceful)
    if got_secs is not None and want_secs is not None:
        graceful_ok = abs(got_secs - want_secs) <= 0.5
    else:
        graceful_ok = graceful == expect_graceful

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
        "acceptq_max": acceptq,
        "bytes_per_stream": (data_bytes / reqs) if reqs else 0.0,
        "graceful_stop": graceful,
        "fingerprint_ok": same_number(ratio, expect_ratio) and graceful_ok,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="汇总定档位压测结果 + 链路预算闸门")
    parser.add_argument("--result-dir", required=True, help="run_levels.sh 的结果目录")
    parser.add_argument("--expect-checks-ratio", type=float, default=DEFAULT_EXPECT_CHECKS_RATIO,
                        help="每档 checks/http_reqs 的期望值（默认 2.00）")
    parser.add_argument("--expect-graceful-stop", default=DEFAULT_EXPECT_GRACEFUL_STOP,
                        help=f"每档日志头 gracefulStop 的期望值（默认 {DEFAULT_EXPECT_GRACEFUL_STOP}）；"
                             "`600s` 与 k6 打印的 `10m0s` 等价，按秒比较。run_levels.sh 会按 "
                             "GRACEFUL_STOP 透传")
    parser.add_argument("--link-mbps", type=float, help="链路协商速率（Mbps，每方向）")
    parser.add_argument("--link-iface", help="本机网卡名（替代 --link-mbps）")
    parser.add_argument("--efficiency", type=float, default=DEFAULT_EFFICIENCY)
    parser.add_argument("--allow-linkbound", action="store_true", help="越界也出报告（仅用于诊断）")
    parser.add_argument("--allow-fingerprint-mismatch", action="store_true",
                        help="指纹不符也出报告（仅用于诊断；默认拒绝）")
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
        row = load_level(args.result_dir, stem, args.expect_checks_ratio, args.expect_graceful_stop)
        target_secs = (row["dur_ms"] or 0) / 1000
        # 闭式（首选）：VUS × dur/iter。缺 iteration_duration 时退化为稀释式下界。
        if row["vus_actual"] and row["iter_ms"]:
            row["active_streams"] = row["vus_actual"] * target_secs / (row["iter_ms"] / 1000)
            row["active_streams_source"] = "closed-form"
        else:
            row["active_streams"] = row["qps"] * target_secs
            row["active_streams_source"] = "diluted-lower-bound"
        row["active_streams_diluted"] = row["qps"] * target_secs
        # 一个 VU 每 iter 秒发一个请求 → 稳态 qps。与实测 qps 的比值就是排空段的稀释程度。
        row["qps_steady"] = (row["vus_actual"] / (row["iter_ms"] / 1000)) if (row["vus_actual"] and row["iter_ms"]) else None

        per_stream = row["bytes_per_stream"]
        if usable and target_secs and per_stream:
            required = row["active_streams"] * per_stream * 8 / target_secs / 1e6
            row["required_mbps"] = required
            bound, text = verdict(required, usable)
            row["link_bound"], row["verdict"] = bound, text
        else:
            row["required_mbps"] = None
            row["link_bound"] = None
            row["verdict"] = "无 payload 字节" if not per_stream else ("未提供链路速率" if not usable else "无 dur")
        levels.append(row)

    # 闸门先判：不过就**不写 meta.json**（"拒绝出报告"的机械含义）。
    bad = [row for row in levels if not row["fingerprint_ok"]]
    bound = [row for row in levels if row["fingerprint_ok"] and row["link_bound"]]
    refused = (bool(bad) and not args.allow_fingerprint_mismatch) or (
        bool(bound) and not args.allow_linkbound
    )

    env = read_env_txt(args.result_dir)
    if not refused:
        meta = {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "result_dir": os.path.abspath(args.result_dir),
            "diagnostic_override": {
                "allow_linkbound": args.allow_linkbound,
                "allow_fingerprint_mismatch": args.allow_fingerprint_mismatch,
            },
            "env": env,
            "contract": {
                "checks_ratio": args.expect_checks_ratio,
                "tolerance": RATIO_TOLERANCE,
                "graceful_stop": args.expect_graceful_stop,
                "link_mbps": link_mbps,
                "efficiency": args.efficiency,
                "active_streams": "VUS × dur/iter（闭式，首选）；active_streams_diluted = qps × dur 仅作对照",
                "invalid_levels": [row["level"] for row in bad],
            },
            "levels": levels,
        }
        with open(os.path.join(args.result_dir, "meta.json"), "w", encoding="utf-8") as handle:
            json.dump(meta, handle, ensure_ascii=False, indent=2)

    print(f"# {os.path.basename(os.path.abspath(args.result_dir))}")
    if refused:
        print("")
        print("> ⛔ **REFUSED**：闸门未过 → 不写 `meta.json`，本表**不可进对标表**。")
        print("> 诊断需要时用 `--allow-linkbound` / `--allow-fingerprint-mismatch` 显式绕过。")
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
        if row["fingerprint_ok"]:
            mark = "✓"
        else:
            mark = f"✗INVALID（期望 {args.expect_checks_ratio:.2f} / {args.expect_graceful_stop}）"
        qs = f"{row['qps_steady']:.1f}" if row["qps_steady"] else "-"
        aq = "-" if row["acceptq_max"] is None else f"{row['acceptq_max']}"
        active = f"{row['active_streams']:.0f}"
        if row["active_streams_source"] == "diluted-lower-bound":
            active += "(下界)"
        print(
            f"| {row['vus']} | {row['qps']:.1f} | {qs} | {dur} | {ttft} | {active} | "
            f"{row['active_streams_diluted']:.0f} | {aq} | {need} | {row['verdict']} | {mark} |"
        )

    # backlog 触顶的直接证据：accept 队列峰值逼近监听 backlog。
    backlog_raw = env.get("GATEWAY_BACKLOG", "")
    if backlog_raw.isdigit():
        backlog = int(backlog_raw)
        hot = [row for row in levels
               if isinstance(row["acceptq_max"], int) and backlog and row["acceptq_max"] >= backlog]
        if hot:
            names = ", ".join(f"{row['level']}(峰值{row['acceptq_max']})" for row in hot)
            print(f"\n⚠️ accept 队列触顶（backlog={backlog}）：{names}", file=sys.stderr)
            print("  → 这一档量到了连接建立排队，不是网关处理能力。加大 backlog 或降低建连突发。", file=sys.stderr)
    elif any(row["acceptq_max"] == "unknown" for row in levels):
        print("\n⚠️ 本机没有 `ss`（非 Linux？）→ accept 队列记 unknown，backlog 触顶无法判定。", file=sys.stderr)

    if refused:
        if bad:
            names = ", ".join(row["level"] for row in bad)
            print(f"\n✗ 指纹不符（checks 比 ≠ {args.expect_checks_ratio:.2f} 或 "
                  f"gracefulStop ≠ {args.expect_graceful_stop}）：{names}", file=sys.stderr)
            print("  → 这批数不是期望的对标喂法。换回仓库版 k6.js、或把 GRACEFUL_STOP 与 "
                  "--expect-graceful-stop 对齐；见 v88-rig.md §1.1/§1.3。", file=sys.stderr)
            return 3
        print(f"\n✗ 有 {len(bound)} 档 LINK-BOUND：这台台架量的是网线，不是网关 → 拒绝出报告。", file=sys.stderr)
        print("  → 换 10G/25G，或把 k6 与网关/mock 同机（loopback）后重测。", file=sys.stderr)
        return 2

    print(f"\nmeta.json 已写入 {os.path.join(args.result_dir, 'meta.json')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
