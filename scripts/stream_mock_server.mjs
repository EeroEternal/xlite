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
const chunks = Number(arg("chunks", "80"));
const intervalMs = Number(arg("interval-ms", "10"));
const ttftMs = Number(arg("ttft-ms", "0"));
const label = arg("label", "stream-mock");

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

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url || "/", `http://${req.headers.host || `${listen}:${port}`}`);
  if (req.method === "GET" && (url.pathname === "/health" || url.pathname === "/v1/health")) {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ status: "ok", label, active, total, chunks, interval_ms: intervalMs }));
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

  try {
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
    for (let i = 0; i < chunks; i += 1) {
      const payload = {
        id,
        object: "chat.completion.chunk",
        created,
        model: reqModel,
        ts: Date.now(),
        choices: [{ index: 0, delta: { content: `c${i} ` }, finish_reason: null }],
      };
      res.write(`data: ${JSON.stringify(payload)}\n\n`);
      if (intervalMs > 0) {
        await sleep(intervalMs);
      }
    }
    res.write(
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
    res.write("data: [DONE]\n\n");
    res.end();
  } finally {
    active -= 1;
  }
});

server.listen(port, listen, () => {
  console.log(
    `[stream-mock] http://${listen}:${port} model=${model} chunks=${chunks} interval_ms=${intervalMs} ttft_ms=${ttftMs}`,
  );
});
