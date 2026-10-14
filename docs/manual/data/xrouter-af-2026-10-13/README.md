# xrouter 单机 A–F 结果数据

本目录与[结果报告](../../xlite-af-results-2026-10-13.md)配套。展示名为 **xrouter**；
执行记录中的网关标识、代码和二进制原名 `xlite` 保持不变，避免改写证据。

## 内容与口径

| 文件 | 内容 |
|---|---|
| `flatlogs/` | 36个正式阶段日志及`all_progress.log`，共37文件；每档保留16片原文 |
| `comparison.csv` / `comparison.md` | 全部36点的整轮QPS、稳态成功QPS、活跃估计和分片分位数范围 |
| `verified-progress.json` | 42条有效原始阶段记录：36正式、6直连预检 |
| `性能测试指标对照表_xrouter_单机_20261014.xlsx` | 原交付Excel，4表、36正式阶段、576正式分片、648个带缓存公式；字节未改 |
| `raw-evidence.tgz` | 完整解压证据树：2881文件，含原始摘要、日志、资源采样、实际执行脚本和钉版二进制 |
| `publication.json` | 覆盖范围、组/批次映射、原批次包哈希、当前证据包与Excel哈希 |
| `SHA256SUMS` | 本目录全部其他文件的SHA256 |

- A/B/C/E来自r10完整组曲线，D/F来自r11完整组曲线；没有同组混拼或虚构的合并`DONE.json`。
- r10的D直连预检客户端OOM失败保留在原始证据中。r11为D/F增加每片9GiB Go软限制，
  其余硬资源、客户端、采样与载荷不变；不是一个统一客户端GC配置的不中断执行。
- 正式阶段请求10,549,562次、完整体采样5,276,182次，传输/非200/采样体错误均为0。
- TTFT为k6首HTTP字节等待，不等同首个非空content。分位数列为16片范围，不是整体分位数。
- Excel中的chunk间隔来自mock生成时间戳，**不是网络到达间隔**；TTFT减500ms及完整时长减理想值，
  都不应冒称净网关开销。Excel来源页保留交付时本地路径，仓库复核使用本目录证据包。
- 原始包r10/r11未再次重复存储，其完整解压文件均包含在证据树的`sources/r10`与`sources/r11`。
  文件内容未改，只将外层tar所有者和修改时间归零；原包哈希保存在`publication.json`。
- 本目录`.gitattributes`仅对原始平铺日志和CSV关闭换行转换及空白检查，
  保留k6输出的对齐空格、末尾空行和CSV的CRLF，不影响代码与文档检查。
- 不包含数据库、运行凭据、用户提供的New API二进制、镜像包或截图。
  `bin/xlite`和`bin/mock-llm`仅为已测Linux二进制的指纹核查，不是新生产版本发布。

## 离线校验与重建日志

在仓库根目录执行，不需要Docker、不启动服务、不重新压测：

```bash
DATA="$PWD/docs/manual/data/xrouter-af-2026-10-13"
WORK="$(mktemp -d)"

# Linux；macOS可改用 shasum -a 256 -c SHA256SUMS
(cd "$DATA" && sha256sum -c SHA256SUMS)
tar -xzf "$DATA/raw-evidence.tgz" -C "$WORK"
mkdir "$WORK/replay"
python3 scripts/loadtest/sonic_af_1013_report.py \
  --results "$WORK/assembled/results" \
  --output-base "$WORK/replay" --xlite-only --group-coverage

diff -qr "$DATA/flatlogs" "$WORK/replay/retest-single-xlite-af50"
cmp "$DATA/comparison.csv" "$WORK/replay/retest-single-xlite-af50-details/comparison.csv"
cmp "$DATA/verified-progress.json" "$WORK/replay/retest-single-xlite-af50-details/verified-progress.json"
```

预期验证42条阶段记录、672份摘要。导出器逐片复算计数、有效性、均值与分位数范围，
校验原执行脚本、二进制、资源/GC配置，以及合并证据与原执行的日志/摘要字节一致性。
目标导出目录存在时会拒绝覆盖，不修改本目录或原证据。

实际负载执行入口为`xlite_af_1013.py`；`sonic_af_1013.py`保留此前暂停的双网关执行能力，
不代表New API全矩阵已经完成。执行环境、参数及失败历史见
[任务记录](../../sonic-af-retest-2026-10-13.md)。
