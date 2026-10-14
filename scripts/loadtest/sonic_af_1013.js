// Matched A-F rig; mock timestamps describe generation, not client chunk arrival.
var http = require('k6/http');
var k6 = require('k6');
var metrics = require('k6/metrics');
var execution = require('k6/execution');
var urls = (__ENV.TARGET_URLS || '').split(',');
var vus = Number(__ENV.VUS);
var outTokens = Number(__ENV.OUT_TOKENS);
var dataLinesExpected = Number(__ENV.EXPECTED_DATA_LINES);
var warmup = Number(__ENV.WARMUP || 30);
var duration = Number(__ENV.DURATION_SECONDS);
var sample = Number(__ENV.KEEP_SAMPLE || 0.5);
var chunkMax = Number(__ENV.CHUNK_MAX || 64);
var timing = __ENV.TIMESTAMP_DIAGNOSTICS === 'true';
if (!Number.isInteger(vus) || vus < 1 || vus >= 20000 ||
    !Number.isInteger(outTokens) || outTokens < 2 || outTokens > 6000 ||
    !Number.isInteger(dataLinesExpected) || dataLinesExpected < outTokens + 2 ||
    dataLinesExpected > outTokens + 5 || !Number.isInteger(chunkMax) || chunkMax < 1 ||
    !Number.isFinite(sample) || sample <= 0 || sample > 1 ||
    !Number.isFinite(warmup) || warmup <= 0 || !Number.isFinite(duration) || duration <= 0 ||
    urls.some(function (url) {
      return !/^http:\/\/(?:af-mock:432(?:0[1-9]|1[0-9]|2[0-4])|af-xlite:18881|af-newapi:18880)\/v1\/chat\/completions$/.test(url);
    })) {
  throw new Error('Invalid matched rig parameters; only isolated task targets and VUS < 20000');
}
var ttft = new metrics.Trend('ttft_ms');
var overhead = new metrics.Trend('gateway_overhead_ms');
var interval = new metrics.Trend('chunk_interval_ms');
var bodySamples = new metrics.Counter('body_samples');
var bodyErrors = new metrics.Counter('body_errors');
var chunkSamples = new metrics.Counter('chunk_samples_added');
var timingSamples = new metrics.Counter('timestamp_diagnostic_bodies');
var successes = new metrics.Counter('successful_requests');
var transportErrors = new metrics.Counter('transport_errors');
var non200 = new metrics.Counter('non200_responses');
var steadyCompleted = new metrics.Counter('steady_completed_requests');
var steadySuccesses = new metrics.Counter('steady_successful_requests');
var steadyTtft = new metrics.Trend('steady_ttft_ms');
var steadyDuration = new metrics.Trend('steady_http_duration_ms');
var payload = JSON.stringify({
  model: 'mock-model',
  messages: [{ role: 'user', content: 'mock input text. '.repeat(25) }],
  stream: true, max_tokens: outTokens, stream_options: { include_usage: true },
});
module.exports.options = {
  discardResponseBodies: true,
  summaryTrendStats: ['p(50)', 'p(90)', 'p(95)', 'p(99)', 'avg', 'min', 'max'],
  scenarios: { load: {
    executor: 'ramping-vus', startVUs: 1,
    stages: [{ target: vus, duration: warmup + 's' }, { target: vus, duration: duration + 's' }],
    gracefulStop: '600s',
  }},
  thresholds: { http_req_failed: ['rate<0.01'], checks: ['rate==1'] },
};
module.exports.default = function () {
  var analyze = Math.random() < sample;
  var response = http.post(urls[(__VU - 1) % urls.length], payload, {
    headers: { 'Content-Type': 'application/json', Authorization: 'Bearer ' + __ENV.TOKEN },
    responseType: analyze ? 'text' : 'none', timeout: '600s',
  });
  ttft.add(response.timings.waiting);
  k6.check(response, { 'status 200': function (r) { return r.status === 200; } });
  if (response.status === 0) transportErrors.add(1);
  else if (response.status !== 200) non200.add(1);
  if (response.status === 200) successes.add(1);
  var elapsed = (Date.now() - execution.scenario.startTime) / 1000;
  if (elapsed >= warmup && elapsed < warmup + duration) {
    steadyCompleted.add(1);
    steadyTtft.add(response.timings.waiting);
    steadyDuration.add(response.timings.duration);
    if (response.status === 200) steadySuccesses.add(1);
  }
  if (analyze) {
    bodySamples.add(1);
    var body = response.body || '';
    var content = body.match(/"content"\s*:\s*"[^"]/g) || [];
    var lines = body.match(/^data: /gm) || [];
    var valid = response.status === 200 && content.length === outTokens &&
      lines.length === dataLinesExpected && body.trim().endsWith('data: [DONE]') &&
      /"finish_reason"\s*:\s*"stop"/.test(body) &&
      new RegExp('"completion_tokens"\\s*:\\s*' + outTokens + '\\s*[,}]').test(body);
    var ts = [];
    if (timing) {
      var re = /"ts"\s*:\s*(\d+)/g;
      var match;
      while ((match = re.exec(body)) !== null) ts.push(Number(match[1]));
      valid = valid && ts.length === outTokens;
    }
    k6.check(response, { 'body ok': function () { return valid; } });
    if (!valid) bodyErrors.add(1);
    else if (timing) {
      timingSamples.add(1);
      overhead.add(response.timings.receiving - (ts[ts.length - 1] - ts[0]));
      var count = Math.min(chunkMax, ts.length - 1);
      for (var i = 0; i < count; i++) {
        var index = 1 + Math.floor(i * (ts.length - 1) / count);
        interval.add(ts[index] - ts[index - 1]);
      }
      chunkSamples.add(count);
    }
  }
  k6.sleep(0);
};
