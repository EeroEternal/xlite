// 1013 B-group single rig. Chunk timestamps measure mock generation, not arrival.
var http = require('k6/http');
var k6 = require('k6');
var metrics = require('k6/metrics');
var execution = require('k6/execution');

var urls = (__ENV.TARGET_URLS || 'http://127.0.0.1:18313/v1/chat/completions').split(',');
var vus = Number(__ENV.VUS || 1000);
var warmup = Number(__ENV.WARMUP || 30);
var duration = Number(__ENV.DURATION_SECONDS || 300);
var sample = Number(__ENV.KEEP_SAMPLE || 1);
var chunkMax = Number(__ENV.CHUNK_MAX || 128);
if (!Number.isInteger(vus) || vus < 1 || vus >= 20000 ||
    !Number.isInteger(chunkMax) || chunkMax < 1 ||
    !Number.isFinite(sample) || sample <= 0 || sample > 1 ||
    !Number.isFinite(warmup) || warmup <= 0 ||
    !Number.isFinite(duration) || duration <= 0 ||
    urls.some(function (url) { return !/^http:\/\/127\.0\.0\.1:\d+\/v1\/chat\/completions$/.test(url); })) {
  throw new Error('Invalid rig parameters; only loopback URLs and VUS < 20000 are supported');
}

var ttft = new metrics.Trend('ttft_ms');
var overhead = new metrics.Trend('gateway_overhead_ms');
var interval = new metrics.Trend('chunk_interval_ms');
var chunkSamples = new metrics.Counter('chunk_samples_added');
var bodySamples = new metrics.Counter('body_samples');
var bodyErrors = new metrics.Counter('body_errors');
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
  stream: true,
  stream_options: { include_usage: true },
});

module.exports.options = {
  discardResponseBodies: true,
  summaryTrendStats: ['p(50)', 'p(90)', 'p(95)', 'p(99)', 'avg', 'min', 'max'],
  scenarios: {
    load: {
      executor: 'ramping-vus',
      startVUs: 1,
      stages: [
        { target: vus, duration: warmup + 's' },
        { target: vus, duration: duration + 's' },
      ],
      gracefulStop: '600s',
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.01'],
    checks: ['rate==1'],
  },
};

module.exports.default = function () {
  var analyze = Math.random() < sample;
  var response = http.post(urls[(__VU - 1) % urls.length], payload, {
    headers: {
      'Content-Type': 'application/json',
      Authorization: 'Bearer ' + (__ENV.TOKEN || 'sk-loadtest'),
    },
    responseType: analyze ? 'text' : 'none',
    timeout: '600s',
  });
  ttft.add(response.timings.waiting);
  k6.check(response, { 'status 200': function (r) { return r.status === 200; } });
  if (response.status === 0) transportErrors.add(1);
  else if (response.status !== 200) non200.add(1);
  if (response.status === 200) successes.add(1);

  // A completion-window counter, separate from k6's whole-run diluted rate.
  var elapsed = (Date.now() - execution.scenario.startTime) / 1000;
  if (elapsed >= warmup && elapsed < warmup + duration) {
    steadyCompleted.add(1);
    steadyTtft.add(response.timings.waiting);
    steadyDuration.add(response.timings.duration);
    if (response.status === 200) steadySuccesses.add(1);
  }

  if (analyze) {
    bodySamples.add(1);
    var ts = [];
    var re = /"ts"\s*:\s*(\d+)/g;
    var match;
    var body = response.body || '';
    while ((match = re.exec(body)) !== null) ts.push(Number(match[1]));
    var contentFrames = body.match(/"content"\s*:\s*"/g) || [];
    var dataLines = body.match(/^data: /gm) || [];
    var valid = response.status === 200 && ts.length === 500 &&
      contentFrames.length === 500 && dataLines.length === 502 &&
      body.trim().endsWith('data: [DONE]') &&
      /"finish_reason"\s*:\s*"stop"/.test(body) &&
      /"completion_tokens"\s*:\s*500\s*[,}]/.test(body);
    k6.check(response, { 'body ok': function () { return valid; } });
    if (!valid) {
      bodyErrors.add(1);
    } else {
      overhead.add(response.timings.receiving - (ts[ts.length - 1] - ts[0]));
      // Uniformly cover the stream instead of biasing samples to its first frames.
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
