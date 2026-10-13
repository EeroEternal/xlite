#!/usr/bin/env python3
"""Render completed 1013 run artifacts against the historical New API logs."""

import argparse
import hashlib
import json
from pathlib import Path
import re


def line(text, name):
    match = re.search(r"^\s*" + re.escape(name) + r"\.+:\s*(.+)$", text, re.MULTILINE)
    if not match:
        raise ValueError(f"Missing historical metric: {name}")
    return match[1]


def stat(text, key):
    match = re.search(re.escape(key) + r"=([^\s]+)", text)
    if not match:
        raise ValueError(f"Missing historical statistic: {key}")
    return match[1]


def seconds(value):
    match = re.fullmatch(r"([0-9.]+)(ms|s)", value)
    if not match:
        raise ValueError(f"Unsupported historical duration: {value}")
    return float(match[1]) / (1000 if match[2] == "ms" else 1)


def historical(folder):
    rows = {}
    for path in sorted(folder.glob("B_*_300s.log")):
        vus = int(path.name.split("_")[1])
        if vus >= 20000:
            continue
        text = path.read_text()
        if "TOTAL RESULTS" not in text:
            continue
        requests = re.fullmatch(r"(\d+)\s+([0-9.]+)/s", line(text, "http_reqs").strip())
        if not requests:
            raise ValueError(f"Unrecognized request count/rate in {path.name}")
        http = seconds(stat(line(text, "http_req_duration"), "avg"))
        iteration = seconds(stat(line(text, "iteration_duration"), "avg"))
        failed = re.match(r"([0-9.]+)%", line(text, "http_req_failed"))
        if not failed:
            raise ValueError(f"Missing failure rate in {path.name}")
        ttft = line(text, "ttft_ms")
        rows[vus] = {
            "qps": float(requests[2]), "ttft_p50": float(stat(ttft, "p(50)")),
            "ttft_p99": float(stat(ttft, "p(99)")), "failure": float(failed[1]),
            "http_avg_s": http, "iter_avg_s": iteration, "active": vus * http / iteration,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        }
    if not rows:
        raise ValueError("No completed historical New API logs")
    return rows


def interval(stats, key):
    bounds = stats.get(key + "_shard_range")
    return f"{bounds[0]:.2f}–{bounds[1]:.2f}" if bounds else "unknown"


def resource_peaks(results, row, shards):
    path = results / (row["label"] + ".resources.jsonl")
    gateway, client, throttles = [], [], []
    if not path.exists():
        return None, None, None
    for line in path.read_text().splitlines():
        containers = json.loads(line).get("containers", [])
        clients = [c.get("cpu_cores") for c in containers if "-k6-" in c["name"]]
        if len(clients) == shards and all(value is not None for value in clients):
            client.append(sum(clients))
        for container in containers:
            if container["name"].endswith("-gateway"):
                value = container.get("cpu_cores")
                if value is not None:
                    gateway.append(value)
                value = container.get("cpu_stat", {}).get("nr_throttled")
                if value is not None:
                    throttles.append(value)
    return (max(gateway) if gateway else None, max(client) if client else None,
            throttles[-1] - throttles[0] if len(throttles) > 1 else None)


def render(results, baseline, previous=None):
    done = json.loads((results / "DONE.json").read_text())
    env = json.loads((results / "env.json").read_text())
    rows = json.loads((results / "progress.json").read_text())
    expected = {("direct", 18000)}
    expected.update(("full", n) for n in [1000, 2000, 5000, 10000, 15000, 18000])
    expected.update(("half", n) for n in [1000, 2000, 5000, 10000])
    actual = [(row["track"], row["vus"]) for row in rows]
    if set(actual) != expected or len(actual) != len(expected) or done["runs"] != len(rows):
        raise ValueError("Incomplete or duplicate planned phases")
    if any(not row["valid"] for row in rows):
        raise ValueError("Invalid phase cannot enter the report")
    for row in rows:
        shards = row["shards"]
        if len(shards) != env["k6_shards"] or sum(part["vus"] for part in shards) != row["vus"]:
            raise ValueError("Shard VU count mismatch")
        if any(part["exit_code"] != 0 or not part["valid"] for part in shards):
            raise ValueError("Invalid shard cannot enter the report")
        expected_sample = 0.5 if row["track"] == "half" else 1
        if row["sample"] != expected_sample:
            raise ValueError("Sampling track mismatch")
    lines = [
        "# 1013 single B 组复测结果", "",
        "## 口径与边界", "",
        f"- 固定源码：`{env['commit']}`；{env['k6_shards']} 个 k6 分片，单个 xlite 网关，24 路 Rust mock，全部同机。",
        f"- 网关 worker：{env.get('gateway_workers', 'unknown')}；CPU 配额：`{json.dumps(env.get('cpu_quotas', 'unknown'))}`。配额单位是逻辑 CPU 时间，不是物理核数。",
        "- B 组：500ms 首块等待、500 个内容帧、10ms 间隔、batch=1；每档 30s 爬坡 + 300s 稳态。",
        "- TTFT 分位数列是各片的最小～最大范围，不是整体分位数；QPS 是各片整轮速率之和。",
        "- 稳态成功 QPS 单独列出，不与历史整轮 QPS 混算；在途数是平均闭式估计，不是 VU 或 TCP 连接数。",
        "- 在途估计逐片使用 VU × HTTP 平均时长 / 迭代平均时长；整轮包含爬坡，不能视为直接测得的稳态或峰值并发。",
        "- valid=true 表示测量窗口、退出与响应校验通过，不表示已达到设定 VU 对应的在途并发；90s 直连预检也不能证明 300s 顶档有相同余量。",
        "- New API 仅为历史参考：缺少原脚本、镜像、硬件及 mock 元数据；约 50% 响应体采样是日志指纹推断。",
        "- 历史日志没有 chunk_samples_added，无法确认其每请求 chunk 样本上限；即使响应体采样同为约 50%，也不代表客户端工作量相同。",
        "- Rust mock 低载约 5.50s，历史 HTTP 时长约 5.83–6.00s；载荷/时序与客户端配置差异不能归功于网关。",
        "- 本报告仅解析 New API 的 1k/2k/5k/10k 参考档位；xlite 没有 20k 同档测试。表中的“无基准”表示未载入匹配参考，不据此判定其他日志不存在或未完成。",
        "- 0% HTTP 失败还必须结合流完整性检查；gateway_overhead 可为负，不作为独立胜负指标。",
        "", "## New API 历史参考", "",
        "| VU | 整轮 QPS | TTFT P50 / P99 (ms) | HTTP / 迭代平均 (s) | 在途估计 | HTTP 失败 |",
        "|---:|---:|---:|---:|---:|---:|",
    ]
    for vus, row in sorted(baseline.items()):
        lines.append(f"| {vus} | {row['qps']:.2f} | {row['ttft_p50']:.2f} / {row['ttft_p99']:.2f} | "
                     f"{row['http_avg_s']:.2f} / {row['iter_avg_s']:.2f} | {row['active']:.0f} | {row['failure']:.2f}% |")
    for track, title in [("full", "100% 响应体诊断采样，128 chunk 样本/请求"),
                         ("half", "约 50% 响应体诊断采样，64 chunk 样本/采样请求")]:
        lines += ["", "## " + title, "",
                  "| VU | 整轮 QPS | 稳态成功 QPS | 在途估计 | TTFT P50 范围 (ms) | TTFT P99 范围 (ms) | HTTP 失败 | 历史 QPS 差 |",
                  "|---:|---:|---:|---:|---:|---:|---:|---:|"]
        for row in rows:
            if row["track"] != track:
                continue
            old = baseline.get(row["vus"])
            change = f"{(row['qps_whole_run'] / old['qps'] - 1) * 100:+.1f}%" if old else "无基准"
            lines.append(f"| {row['vus']} | {row['qps_whole_run']:.2f} | {row['steady_success_qps']:.2f} | "
                         f"{row['estimated_active']:.0f} | {interval(row['ttft_ms'], 'p(50)')} | "
                         f"{interval(row['ttft_ms'], 'p(99)')} | {row['failure_rate'] * 100:.2f}% | {change} |")
    lines += ["", "## 吞吐差异的限制", ""]
    for row in rows:
        old = baseline.get(row["vus"])
        if row["track"] != "half" or not old:
            continue
        http = row["http_duration_ms"]["avg"] / 1000
        lines.append(f"- {row['vus']} VU：历史 HTTP 平均 {old['http_avg_s']:.3f}s，本次 {http:.3f}s；"
                     f"单凭服务时长变化对应的理想闭式速率比为 {old['http_avg_s'] / http:.3f}×。"
                     "这不是网关性能归因，也不是可直接相减的独立效应。")
    lines += ["", "## 顶档负载与资源限制", "",
              "| VU | 在途估计 / 设定 VU | 网关 CPU 峰值 | k6 合计 CPU 峰值 | 网关节流周期增量 |",
              "|---:|---:|---:|---:|---:|"]
    for row in rows:
        if row["track"] != "full":
            continue
        gateway, client, throttles = resource_peaks(results, row, env["k6_shards"])
        g = f"{gateway:.2f}" if gateway is not None else "未知"
        c = f"{client:.2f}" if client is not None else "未知"
        t = str(throttles) if throttles is not None else "未知"
        lines.append(f"| {row['vus']} | {row['estimated_active'] / row['vus'] * 100:.1f}% | {g} | {c} | {t} |")
    lines += ["", "- CPU 列为资源快照峰值，不是稳态均值；缺失样本不补零，k6 合计只使用全部分片 CPU 均可见的快照。",
              "- 高档位必须结合在途比例和节流看待。设定 18k VU 不等于已证明稳定承载 18k 在途请求；网关/客户端资源预算限制下的平台也不是无约束的产品极限。"]
    if previous is not None:
        old_env = json.loads((previous / "env.json").read_text())
        old_rows = json.loads((previous / "progress.json").read_text())
        lines += ["", "## 旧配置失败记录（不拼接到本轮曲线）", "",
                  f"- 旧网关 worker：{old_env.get('gateway_workers', 'unknown')}；CPU 配额：`{json.dumps(old_env.get('cpu_quotas', 'unknown'))}`。",
                  "- 下列 HTTP QPS 包含失败，不能称为成功吞吐；EOF、节流和协议诊断的完整记录见测试任务文档。",
                  "",
                  "| 阶段 | HTTP QPS（含失败） | HTTP 失败率 | 无效原因 |",
                  "|---|---:|---:|---|"]
        for row in old_rows:
            if not row["valid"]:
                reasons = "; ".join(sorted(set(row["invalid_reasons"]))).replace("|", "/")
                lines.append(f"| {row['label']} | {row['qps_whole_run']:.2f} | "
                             f"{row['failure_rate'] * 100:.2f}% | {reasons} |")
        lines.append("- 增加资源后若复测通过，只能说明该配置下结果改善；多个资源变量同时变化，不能据此单独锁定 EOF 根因。")
    lines += ["", "## 验证与工件", "",
              f"- 全部 {len(rows)} 个计划阶段有效完成；每片退出码、响应完整性和采样指纹校验通过。",
              "- full/half 两条线分别报告，不拼接；前三轮失败/中断预检不进入本报告。",
              "- 每片原始 summary 保留精确分位数；progress.json 保存计数、加权均值与分片范围。",
              "- 每档 *.resources.jsonl 保存 CPU 增量、节流计数、容器内存和查询缺失，不把未知补成 0。",
        f"- 本轮实际使用的驱动器 SHA256：`{env['driver_sha256']}`；不是运行过程中修改后的本地工作文件。",
              f"- k6 image ID：`{env['k6_image_id']}`。", "",
              "### 历史输入哈希", "",
              "| VU | SHA256 |", "|---:|---|"]
    for vus, row in sorted(baseline.items()):
        lines.append(f"| {vus} | `{row['sha256']}` |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--previous-results", type=Path,
                        help="Include failed phases from a separate resource configuration")
    args = parser.parse_args()
    text = render(args.results, historical(args.baseline), args.previous_results)
    with args.output.open("x") as stream:
        stream.write(text)
    print(f"Wrote completed comparison: {args.output}")


if __name__ == "__main__":
    main()
