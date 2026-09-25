#!/usr/bin/env python3
"""链路预算预检：这轮压测的物理链路扛不扛得住？

回答一个问题：k6 / 网关 / mock 之间的**物理链路**，能不能在目标流时长内把这份负载
送完？扛不住时，跑出来的就是网卡的数，不是网关的数 —— 双机 1 GbE 台架在 4 万长流上
只能到 ~890 rps，天花板正好是链路（`docs/manual/v88-rig.md` §2/§7：1 GbE 上组 B
约 5900 路活跃流才不切片）。

模型
----
`vus` 条并发流，每条 `chunks` 块、每块 `bytes_per_chunk` 字节，要求在 `target_secs`
内送完，则每条被 payload 穿过的物理链路需要

    required_mbps = vus * chunks * bytes_per_chunk * 8 / target_secs / 1e6

（Mbps，**每方向**）。一跳到客户端的路径上，同一份 payload 每个方向各穿一次；全双工
链路每方向提供 `link_mbps`。实用上限约 `0.9 * link_mbps`（TCP/IP 开销；`--efficiency`
可调）。`required > 上限` 判 `LINK-BOUND`。

用法
----
    # V88 B 组：500 块 × ~201 B，mock 物理时长 ~5.9s，1 GbE
    scripts/loadtest/link_budget.py --vus 5900 --chunks 500 --bytes-per-chunk 201 --target-secs 5.9 --link-mbps 1000

    # 直接读本机网卡速率
    scripts/loadtest/link_budget.py --vus 5900 --chunks 500 --target-secs 5.9 --link-iface ens35f0np0

    # 自证：夹具必须判 LINK-BOUND / OK
    scripts/loadtest/link_budget.py --selftest

退出码：0 = 扛得住，2 = LINK-BOUND（扛不住），1 = 用法错误。
`scripts/loadtest/report.py` 复用本文件的 `required_mbps` / `usable_mbps` / `verdict`。
"""

from __future__ import annotations

import argparse
import sys

# 实测：仓库 mock `ttft=500ms / 500 块 / 10ms` 的 SSE body ≈ 100.7 KB / 流，
# 即 ~201 B/块（含 `data: {...}\n\n` 框架）。仅作默认值，按实际 mock / 实测覆盖。
DEFAULT_BYTES_PER_CHUNK = 201
# 实用带宽系数：1 GbE 在 TCP/IP 下实测约 110–117 MB/s（88–94%）。
DEFAULT_EFFICIENCY = 0.9


def required_mbps(vus: int, chunks: int, bytes_per_chunk: int, target_secs: float) -> float:
    """每条被 payload 穿过的链路所需带宽（Mbps，每方向）。"""
    if vus <= 0 or chunks <= 0 or bytes_per_chunk <= 0 or target_secs <= 0:
        raise ValueError("vus / chunks / bytes_per_chunk / target_secs 必须为正")
    return vus * chunks * bytes_per_chunk * 8 / target_secs / 1e6


def usable_mbps(link_mbps: float, efficiency: float = DEFAULT_EFFICIENCY) -> float:
    """扣掉协议开销后每方向可用的带宽（Mbps）。"""
    if link_mbps <= 0:
        raise ValueError("link_mbps 必须为正")
    if not 0 < efficiency <= 1:
        raise ValueError("efficiency 必须在 (0, 1]")
    return link_mbps * efficiency


def read_link_mbps(iface: str) -> float:
    """从本机 sysfs 读网卡协商速率（Mbps）。"""
    path = f"/sys/class/net/{iface}/speed"
    try:
        with open(path, encoding="ascii") as handle:
            return float(handle.read().strip())
    except OSError as exc:
        raise SystemExit(f"✗ 读不到 {path}（网卡名是否正确？非本机请用 --link-mbps）: {exc}")


def verdict(required: float, usable: float) -> tuple[bool, str]:
    """返回 (is_link_bound, 一行判读)。"""
    ratio = required / usable if usable else float("inf")
    if ratio > 1.0:
        return True, f"LINK-BOUND：需要 {required:,.0f} Mbps > 可用 {usable:,.0f} Mbps（{ratio:.1f}× 超预算）"
    if ratio > 0.8:
        return False, f"接近链路边界：需要 {required:,.0f} Mbps，可用 {usable:,.0f} Mbps（已用 {ratio * 100:.0f}%）"
    return False, f"OK：需要 {required:,.0f} Mbps，可用 {usable:,.0f} Mbps（{ratio * 100:.0f}%）"


def _selftest() -> int:
    """夹具自证：判定必须能红能绿（先说出什么会让它失败）。"""
    cases = [
        # V88 B 组：1 GbE 预算约 5900 路活跃流（v88-rig.md §7），5900 应 OK
        (dict(vus=5900, chunks=500, bytes_per_chunk=201, target_secs=5.9, link_mbps=1000), False),
        # 同一负载到 7000 路已越界（1 GbE 装不下）
        (dict(vus=7000, chunks=500, bytes_per_chunk=201, target_secs=5.9, link_mbps=1000), True),
        # 同样的 31000 路在 10 GbE 上应 OK
        (dict(vus=31000, chunks=500, bytes_per_chunk=201, target_secs=5.9, link_mbps=10000), False),
        # 31000 路在 1 GbE 上必然 LINK-BOUND
        (dict(vus=31000, chunks=500, bytes_per_chunk=201, target_secs=5.9, link_mbps=1000), True),
    ]
    failed = 0
    for kwargs, want_bound in cases:
        req = required_mbps(kwargs["vus"], kwargs["chunks"], kwargs["bytes_per_chunk"], kwargs["target_secs"])
        bound, _ = verdict(req, usable_mbps(kwargs["link_mbps"]))
        tag = "LINK-BOUND" if bound else "OK"
        ok = bound == want_bound
        failed += 0 if ok else 1
        print(f"  [{'ok' if ok else 'FAIL'}] {kwargs['vus']:>6} VU @ {kwargs['link_mbps']:>5} Mbps -> {tag}")
    if failed:
        print(f"✗ 自测失败 {failed} 例", file=sys.stderr)
        return 1
    print("✓ 自测通过")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="压测台架链路预算预检（判是否 LINK-BOUND）")
    parser.add_argument("--vus", type=int, help="并发流数（VU）")
    parser.add_argument("--chunks", type=int, help="每条流的 SSE 块数")
    parser.add_argument("--bytes-per-chunk", type=int, default=DEFAULT_BYTES_PER_CHUNK,
                        help=f"每块字节数（默认 {DEFAULT_BYTES_PER_CHUNK}，含 SSE 框架）")
    parser.add_argument("--target-secs", type=float, help="期望单流时长（秒）")
    parser.add_argument("--link-mbps", type=float, help="链路协商速率（Mbps，每方向）")
    parser.add_argument("--link-iface", help="本机网卡名，从 sysfs 自动读速率（替代 --link-mbps）")
    parser.add_argument("--efficiency", type=float, default=DEFAULT_EFFICIENCY,
                        help=f"协议开销系数（默认 {DEFAULT_EFFICIENCY}）")
    parser.add_argument("--selftest", action="store_true", help="跑夹具自证")
    args = parser.parse_args(argv)

    if args.selftest:
        return _selftest()

    missing = [name for name in ("vus", "chunks", "target_secs") if getattr(args, name) is None]
    if args.link_mbps is None and args.link_iface is None:
        missing.append("link_mbps/link_iface")
    if missing:
        parser.error("缺少参数：" + ", ".join("--" + m.replace("_", "-") for m in missing))

    link_mbps = args.link_mbps if args.link_mbps is not None else read_link_mbps(args.link_iface)
    req = required_mbps(args.vus, args.chunks, args.bytes_per_chunk, args.target_secs)
    usable = usable_mbps(link_mbps, args.efficiency)
    bound, line = verdict(req, usable)

    print(f"负载：{args.vus} VU × {args.chunks} 块 × {args.bytes_per_chunk} B / {args.target_secs}s")
    print(f"链路：{link_mbps:,.0f} Mbps（实用 {usable:,.0f} Mbps，系数 {args.efficiency}）")
    print(("✗ " if bound else "✓ ") + line)
    if bound:
        print("  → 这台台架量的是网卡，不是网关。换 10G/25G 或把网关与 mock/k6 同机（loopback）后重测。")
        print("  → 详见 docs/manual/v88-rig.md §2 / §7")
    return 2 if bound else 0


if __name__ == "__main__":
    sys.exit(main())
