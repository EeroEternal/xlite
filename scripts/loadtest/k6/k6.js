// New API 网关压测脚本 v3.2-memopt (内存优化版)
// 优化点:
//   [1] 默认丢弃响应体: responseType:'none' + discardResponseBodies:true
//       (SSE 长流 body 全量缓冲 ≈1MB/VU/60s流, 是最大内存项)
//   [2] chunk_interval_ms / gateway_overhead_ms 默认关闭
//       (6000 Trend 样本/迭代, 实测最大内存黑洞), 仅诊断模式开启
//   [3] require()/module.exports 语法 — 兼容 --compatibility-mode=base
//       (base 模式省去每 VU 的 babel 运行时)
//   [4] payload 模块级构建一次, 迭代间复用 (600B/VU, 可忽略)
// TTFT (timings.waiting) 不依赖响应体, 丢 body 后仍然保留
// 客户对标喂法 (与 run_full_88.sh 那轮出数的版本一致, 见 docs/manual/v88-rig.md §1.1):
//   KEEP_BODY=true ⇒ **每次迭代全量解析 body** (checks_total/http_reqs = 2.00) +
//   chunk_interval_ms / gateway_overhead_ms / gracefulStop 600s。
//   KEEP_SAMPLE 默认 **1**(全量); 要省内存显式传 KEEP_SAMPLE=0.1 ——
//   那等于客户后来的 v3.2-memopt 版 (采样 10%, checks 比 1.10), 只在内存吃紧时才用,
//   对标报数请用默认值。
// 环境变量:
//   TARGET_URL / TOKEN / MODEL / STREAM / IN_TOKENS / OUT_TOKENS
//   VUS / DURATION / MAX_VUS / WARMUP(默认30) / RAMP_MODE(linear/burst)
//   STREAM_TIMEOUT(默认600000) / KEEP_BODY(默认false) / GRACEFUL_STOP(默认600s)
//   KEEP_SAMPLE(默认1=全量; 0.1=memopt 采样) / PICK_INTERVAL
//   ERR_SAMPLE(默认0.01) 失败响应体采样率: 从 429 body 提取 error.type /
//     queue_wait_ms / queue_position 并计数, 同时始终采集 Retry-After 头 ——
//     用于区分「队列满 / 排队超时 / 并发到顶 / 限流」

var http = require('k6/http');
var k6mod = require('k6');
var metricsMod = require('k6/metrics');

var KEEP_BODY = ((__ENV.KEEP_BODY || 'false') === 'true');
var GRACEFUL_STOP = __ENV.GRACEFUL_STOP || '600s';
// KEEP_BODY 解析采样率。默认 1 = 每次迭代都解析 (客户对标喂法, checks 比 2.00);
// 显式传 0.1 复刻客户 v3.2-memopt (checks 比 1.10), 0 = 只留状态码校验。
var KEEP_SAMPLE = parseFloat(__ENV.KEEP_SAMPLE || '1');
var ERR_SAMPLE = parseFloat(__ENV.ERR_SAMPLE || '0.01');    // 失败响应体采样率(默认1%)

var ttftMs = new metricsMod.Trend('ttft_ms');
// 响应头拆解：不依赖 KEEP_BODY，每请求各 1 个样本（远小于 chunk 指标）。
var queueWaitMs = new metricsMod.Trend('queue_wait_ms');
var upstreamTtftMs = new metricsMod.Trend('upstream_ttft_ms');
var retryAfterS = new metricsMod.Trend('retry_after_s');
// 失败响应体（仅 ERR_SAMPLE 采样）：区分拒绝原因
var errQueueWaitMs = new metricsMod.Trend('err_queue_wait_ms');
var errQueuePos = new metricsMod.Trend('err_queue_position');
var cntQueueFull = new metricsMod.Counter('err_api_key_queue_full');
var cntQueueTimeout = new metricsMod.Counter('err_api_key_queue_timeout');
var cntTenantQueueFull = new metricsMod.Counter('err_api_key_tenant_queue_full');
var cntConcurrency = new metricsMod.Counter('err_api_key_concurrency_exceeded');
var cntKeyRate = new metricsMod.Counter('err_api_key_rate_limit_exceeded');
var cntGlobalRate = new metricsMod.Counter('err_rate_limit_exceeded');
var cntOther = new metricsMod.Counter('err_other');
// 与客户原件同构: 只有 KEEP_BODY 才注册 chunk 指标 (KEEP_BODY=false 时完全不解析 body,
// checks 比 1.00 —— 满负荷"3 万连接"那一臂就是这一档)。
var chunkIntervalMs = KEEP_BODY ? new metricsMod.Trend('chunk_interval_ms') : null;
var gatewayOverheadMs = KEEP_BODY ? new metricsMod.Trend('gateway_overhead_ms') : null;

var TARGET_URL = __ENV.TARGET_URL || 'http://127.0.0.1:8004/v1/chat/completions';
var TOKEN = __ENV.TOKEN || 'sk-test';
var MODEL = __ENV.MODEL || 'mock-model';
var STREAM = ((__ENV.STREAM || 'false') === 'true');
var IN_TOKENS = parseInt(__ENV.IN_TOKENS || '100', 10);
var OUT_TOKENS = parseInt(__ENV.OUT_TOKENS || '50', 10);
var VUS = parseInt(__ENV.VUS || '100', 10);
var DURATION = __ENV.DURATION || '60s';
var MAX_VUS = parseInt(__ENV.MAX_VUS || '10000', 10);
var WARMUP = parseInt(__ENV.WARMUP || '30', 10);
var RAMP_MODE = (__ENV.RAMP_MODE || 'linear').trim();

function padChars(n) {
  var unit = 'mock input text. ';
  var s = '';
  while (s.length < n) s += unit;
  return s.slice(0, n);
}

function extractTs(body) {
  // 匹配 "ts":数字 或 "ts": 数字
  var re = /"ts"\s*:\s*(\d+)/g;
  var out = [];
  var m;
  while ((m = re.exec(body)) !== null) {
    out.push(parseInt(m[1], 10));
  }
  return out;
}

function headerValue(res, name) {
  var h = res.headers || {};
  var want = name.toLowerCase();
  for (var k in h) {
    if (k.toLowerCase() === want) return String(h[k]);
  }
  return '';
}

function addHeaderTrend(trend, res, name) {
  var raw = headerValue(res, name);
  if (!raw) return;
  var n = parseInt(raw, 10);
  if (isFinite(n) && n >= 0) trend.add(n);
}

// 失败响应体(429)形如: {"error":{"message":...,"type":"api_key_queue_full",
//   "queue_wait_ms":123,"queue_position":45}} —— 用正则提取, 不抛异常。
function jsonStr(body, key) {
  var m = new RegExp('"' + key + '"\\s*:\\s*"([^"]*)"').exec(body);
  return m ? m[1] : '';
}
function jsonNum(body, key) {
  var m = new RegExp('"' + key + '"\\s*:\\s*(\\d+)').exec(body);
  return m ? parseInt(m[1], 10) : null;
}
function countReject(body) {
  var t = jsonStr(body, 'type');
  if (t === 'api_key_queue_full') cntQueueFull.add(1);
  else if (t === 'api_key_queue_timeout') cntQueueTimeout.add(1);
  else if (t === 'api_key_tenant_queue_full') cntTenantQueueFull.add(1);
  else if (t === 'api_key_concurrency_exceeded') cntConcurrency.add(1);
  else if (t === 'api_key_rate_limit_exceeded') cntKeyRate.add(1);
  else if (t === 'rate_limit_exceeded') cntGlobalRate.add(1);
  else cntOther.add(1);
}

var payload = JSON.stringify({
  model: MODEL,
  messages: [{ role: 'user', content: padChars(IN_TOKENS * 4) }],
  stream: STREAM,
  stream_options: { include_usage: true },
});

function doRequest() {
  // 客户原件口径: 只在 KEEP_BODY 时解析 body; KEEP_SAMPLE 控制采样比例 (默认 1 = 全量)。
  var analyze = KEEP_BODY && (Math.random() < KEEP_SAMPLE);
  var errSample = ERR_SAMPLE > 0 && (Math.random() < ERR_SAMPLE);
  var res = http.post(TARGET_URL, payload, {
    headers: {
      'Content-Type': 'application/json',
      Authorization: 'Bearer ' + TOKEN,
    },
    responseType: (analyze || errSample) ? 'text' : 'none',
    timeout: STREAM ? (__ENV.STREAM_TIMEOUT || '600000') : (__ENV.TIMEOUT || '30000'),
  });

  ttftMs.add(res.timings.waiting);
  addHeaderTrend(queueWaitMs, res, 'x-xlite-queue-wait-ms');
  addHeaderTrend(upstreamTtftMs, res, 'x-xlite-upstream-ttft-ms');
  addHeaderTrend(retryAfterS, res, 'retry-after');

  if (res.status !== 200 && errSample && res.body) {
    countReject(res.body);
    var eQw = jsonNum(res.body, 'queue_wait_ms');
    if (eQw !== null) errQueueWaitMs.add(eQw);
    var eQp = jsonNum(res.body, 'queue_position');
    if (eQp !== null) errQueuePos.add(eQp);
  }

  if (analyze) {
    // 全量/采样诊断模式: 完整校验 + chunk 指标 (KEEP_SAMPLE 比例; 对标用默认 1)
    k6mod.check(res, {
      'status 200': function (r) { return r.status === 200; },
      'body ok': function (r) {
        return STREAM ? r.body.indexOf('[DONE]') !== -1 : r.body.indexOf('choices') !== -1;
      },
    });
    if (STREAM && res.status === 200) {
      var tsArr = extractTs(res.body);
      if (tsArr.length >= 2) {
        for (var i = 1; i < tsArr.length; i++) {
          chunkIntervalMs.add(tsArr[i] - tsArr[i - 1]);
        }
        var backendStreamDur = tsArr[tsArr.length - 1] - tsArr[0];
        var clientStreamDur = res.timings.receiving;
        if (isFinite(backendStreamDur) && isFinite(clientStreamDur)) {
          gatewayOverheadMs.add(clientStreamDur - backendStreamDur);
        }
      }
    }
  } else {
    // 常规模式: 仅状态码校验 (http_req_failed 由 k6 默认回调统计 4xx/5xx)
    k6mod.check(res, {
      'status 200': function (r) { return r.status === 200; },
    });
  }

  if (parseInt(__ENV.PICK_INTERVAL || '1', 10) > 0) k6mod.sleep(0);
}

var stages;
if (RAMP_MODE === 'burst') {
  stages = [
    { target: VUS, duration: '3s' },
    { target: VUS, duration: DURATION },
  ];
} else if (WARMUP > 0) {
  stages = [
    { target: VUS, duration: WARMUP + 's' },
    { target: VUS, duration: DURATION },
  ];
} else {
  stages = [
    { target: VUS, duration: '1s' },
    { target: VUS, duration: DURATION },
  ];
}

module.exports.default = doRequest;
module.exports.options = {
  discardResponseBodies: !KEEP_BODY,
  scenarios: {
    load: {
      executor: 'ramping-vus',
      startVUs: 1,
      stages: stages,
      gracefulStop: GRACEFUL_STOP,
    },
  },
};
