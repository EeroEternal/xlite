#!/usr/bin/env node
import http from "node:http";
import { randomUUID } from "node:crypto";

const args = process.argv.slice(2);
function arg(name, fallback) {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && i + 1 < args.length ? args[i + 1] : fallback;
}

const port = Number(arg("port", "4101"));
const listen = arg("listen", "0.0.0.0");
const model = arg("model", "mock");
const label = arg("label", "stream-mock");

// 运行时可改的配置。`_config` 会就地改这个对象，**后续请求**立刻按新值走
// （harness 的用法就是「先下发、再开打」）。字段名与压测 rig 一致：
//   chunks    ← `out_tokens`
//   intervalMs← `token_interval`
//   ttftMs    ← `ttft`
//   batch     ← `batch`（每 N 个 chunk 合并成一次 write，见 §batch）
const cfg = {
  chunks: Number(arg("chunks", "80")),
  intervalMs: Number(arg("interval-ms", "10")),
  ttftMs: Number(arg("ttft-ms", "0")),
  batch: Number(arg("batch", "1")),
  // 故障注入（failover / 半途断流用例）——网关的 `should_failover`（429/5xx 且未发字节
  // ⇒ 换通道）与「上游半途死掉」此前对本 mock 完全不可测：mock 永远 200、永远发完。
  //   failRate   ← `fail_rate`（0~100：多少比例的 chat 请求直接回 failStatus）
  //   failStatus ← `fail_status`（400~599；failover 只认 429/5xx）
  //   abortAfter ← `abort_after`（>0 时流式发满 N 个内容块后直接断连接；0 = 不断）
  failRate: Number(arg("fail-rate", "0")),
  failStatus: Number(arg("fail-status", "500")),
  abortAfter: Number(arg("abort-after", "0")),
};

function readBody(req) {
  return new Promise((resolve, reject) => {
    let body = "";
    req.on("data", (chunk) => {
      body += chunk;
    });
    req.on("end", () => resolve(body));
    req.on("error", reject);
  });
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

let active = 0;
let total = 0;
// 流式 SSE body 的 `write` 次数累计（进程级）。用来**实测** `batch` 真的减少了 write，
// 而不是只靠读代码断言 —— 自证脚本就比 batch=1 与 batch=5 的增量。
let writes = 0;

// ---------------------------------------------------------------- `_config`
// 压测 rig（`scripts/loadtest/*.sh`）用 `POST /_config` 读写 mock 参数：
//   读：`POST /_config`（无查询串）→ 回一份 JSON
//   写：`POST /_config?batch=10&out_tokens=500&…` → 应用后回同一份 JSON
//
// ⚠️ **`batch` 必须是 JSON 的最后一个键**：harness 用 `grep -o '"batch":[0-9]*}'`
// 取值，它要求 `"batch":N` 后面**紧跟 `}`**。键序一变 harness 就读不到，会判成
// 「配置没生效」并 fail-closed 退出（run_levels.sh / ab_configs.sh / ab_topology.sh）。
// 这是实验室那台 Go mock 的既有约定，见
// docs/dev/loadtest-newapi-controlled-2026-09.md §14。
function configJson() {
  return JSON.stringify({
    model,
    ttft: cfg.ttftMs,
    token_interval: cfg.intervalMs,
    out_tokens: cfg.chunks,
    mix: 0,
    mode: 2,
    fail_rate: cfg.failRate,
    fail_status: cfg.failStatus,
    abort_after: cfg.abortAfter,
    batch: cfg.batch, // ← 必须最后
  });
}

// 只接受整数；非整数交给调用方拒。
function intParam(raw) {
  return /^-?\d+$/.test(raw) ? Number(raw) : null;
}

// 应用查询串。返回 null = 已生效；返回字符串 = 要 400 的原因。
// **不认识、不能兑现、超出范围的一律显式拒绝**，绝不静默忽略 —— 「静默」正是这台
// 台架翻过车的地方（文档里那句「我以为我设了 batch」）。
function applyConfig(url) {
  // key → [下界, 上界, 要写入 cfg 的字段（null = 本 mock 无对应实现）]
  const PARAMS = {
    ttft: [0, 600000, "ttftMs"],
    token_interval: [0, 60000, "intervalMs"],
    out_tokens: [1, 100000, "chunks"],
    batch: [1, 10000, "batch"],
    mix: [0, 0, null], // 本 mock 没有 mix 实现，只接受 harness 传的 mix=0
    mode: [2, 2, null], // 本 mock 只有 harness 用的 mode=2（流式）
    fail_rate: [0, 100, "failRate"],
    fail_status: [400, 599, "failStatus"],
    abort_after: [0, 100000, "abortAfter"],
  };
  const seen = new Set();
  for (const [key, raw] of url.searchParams) {
    if (!(key in PARAMS)) return `unknown config key '${key}'`;
    if (seen.has(key)) return `duplicate config key '${key}'`;
    seen.add(key);
    const value = intParam(raw);
    if (value === null) return `${key} must be an integer, got '${raw}'`;
    const [lo, hi, field] = PARAMS[key];
    if (value < lo || value > hi) {
      const note = lo === hi ? `only ${lo} is supported here` : `out of range [${lo}, ${hi}]`;
      return `${key}=${value} ${note}`;
    }
    if (field !== null) cfg[field] = value;
  }
  return null;
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url || "/", `http://${req.headers.host || `${listen}:${port}`}`);
  if (req.method === "GET" && (url.pathname === "/health" || url.pathname === "/v1/health")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(
      JSON.stringify({
        status: "ok",
        label,
        active,
        total,
        // 读**当前生效值**，不是启动值 —— `_config` 改过之后这里必须跟着变
        chunks: cfg.chunks,
        interval_ms: cfg.intervalMs,
        ttft_ms: cfg.ttftMs,
        batch: cfg.batch,
        fail_rate: cfg.failRate,
        fail_status: cfg.failStatus,
        abort_after: cfg.abortAfter,
        writes,
      }),
    );
    return;
  }
  if (url.pathname === "/_config" && (req.method === "POST" || req.method === "GET")) {
    if ([...url.searchParams.keys()].length === 0) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(configJson()); // 读
      return;
    }
    const failure = applyConfig(url);
    if (failure !== null) {
      res.writeHead(400, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: failure } }));
      return;
    }
    console.log(`[stream-mock] :${port} _config applied ${url.search}`);
    res.writeHead(200, { "content-type": "application/json" });
    res.end(configJson()); // 写完回读同一份
    return;
  }
  if (req.method === "GET" && (url.pathname === "/models" || url.pathname === "/v1/models")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ object: "list", data: [{ id: model, object: "model", owned_by: label }] }));
    return;
  }
  if (
    req.method !== "POST" ||
    (url.pathname !== "/chat/completions" && url.pathname !== "/v1/chat/completions")
  ) {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { message: `unsupported ${url.pathname}` } }));
    return;
  }

  let parsed = {};
  try {
    const raw = await readBody(req);
    parsed = raw ? JSON.parse(raw) : {};
  } catch {
    res.writeHead(400, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: { message: "invalid json" } }));
    return;
  }

  const stream = parsed.stream === true;
  const reqModel = typeof parsed.model === "string" ? parsed.model : model;
  active += 1;
  total += 1;
  const id = `chatcmpl-${randomUUID()}`;
  const created = Math.floor(Date.now() / 1000);

  // 每个请求取一次「当前生效配置」的快照：`_config` 只影响**之后的**请求，
  // 不会把正在飞的流改半截。
  const { ttftMs, intervalMs, chunks, batch, failRate, failStatus, abortAfter } = cfg;

  try {
    // 故障注入在**写响应头之前**判定：failover 只对「未发过字节的 429/5xx」换通道
    // （src/upstream.rs `should_failover`），所以错误必须是完整的错误响应而不是断流。
    // fail_rate=100 ⇒ 确定性全挂（自证用例就靠它）。
    if (failRate > 0 && Math.random() * 100 < failRate) {
      res.writeHead(failStatus, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: `mock injected failure ${failStatus}` } }));
      return;
    }
    if (ttftMs > 0) {
      await sleep(ttftMs);
    }
    if (!stream) {
      await sleep(chunks * intervalMs);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          id,
          object: "chat.completion",
          created,
          model: reqModel,
          choices: [
            { index: 0, finish_reason: "stop", message: { role: "assistant", content: "ok" } },
          ],
          usage: { prompt_tokens: 10, completion_tokens: chunks, total_tokens: chunks + 10 },
        }),
      );
      return;
    }

    res.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });

    // `batch=N`：把 N 个 chunk 拼起来一次 `write`。**载荷一个字节都不改** ——
    // 拼的仍是同样的 `data: {...}\n\n` 串，只是 write 次数少了。
    // 本 mock 自己的实测在 scripts/loadtest/test_stream_mock_config.sh（同 22 帧：
    // batch=1 → 22 次 write，batch=5 → 5 次；字节数 / data: 行数 / [DONE] 全等）；
    // Go 农场那组（500 chunk、100 734 B / 502 行）见
    // docs/dev/loadtest-newapi-controlled-2026-09.md §14.1。
    let pending = "";
    let sinceFlush = 0;
    const write = (text) => {
      writes += 1;
      res.write(text);
    };
    const emit = (text) => {
      pending += text;
      if (++sinceFlush >= batch) {
        write(pending);
        pending = "";
        sinceFlush = 0;
      }
    };

    for (let i = 0; i < chunks; i += 1) {
      const payload = {
        id,
        object: "chat.completion.chunk",
        created,
        model: reqModel,
        ts: Date.now(),
        choices: [{ index: 0, delta: { content: `c${i} ` }, finish_reason: null }],
      };
      emit(`data: ${JSON.stringify(payload)}\n\n`);
      if (abortAfter > 0 && i + 1 >= abortAfter) {
        if (pending) write(pending);
        // `res.destroy()` 会把缓冲里没发的字节一起丢掉（实测客户端收到 0 字节）；
        // 半关闭 socket：已写的字节先冲出去，然后 FIN —— 对端看到的是「带数据的
        // 截断流」（curl error 18），正是上游半途死掉的形态，无 finish / [DONE]。
        res.socket?.end();
        return;
      }
      if (intervalMs > 0) {
        await sleep(intervalMs);
      }
    }
    emit(
      `data: ${JSON.stringify({
        id,
        object: "chat.completion.chunk",
        created,
        model: reqModel,
        ts: Date.now(),
        choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
        usage: { prompt_tokens: 10, completion_tokens: chunks, total_tokens: chunks + 10 },
      })}\n\n`,
    );
    emit("data: [DONE]\n\n");
    if (pending) {
      write(pending); // 末尾不足一个 batch 的收尾
    }
    res.end();
  } finally {
    active -= 1;
  }
});

server.listen(port, listen, () => {
  console.log(
    `[stream-mock] http://${listen}:${port} model=${model} chunks=${cfg.chunks}` +
      ` interval_ms=${cfg.intervalMs} ttft_ms=${cfg.ttftMs} batch=${cfg.batch}`,
  );
});
