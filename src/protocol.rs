//! Multi-protocol translation engine ported from `unigateway-core` and
//! `unigateway-protocol` (`Anthropic /v1/messages`, `OpenAI /v1/responses`,
//! and `OpenAI /v1/embeddings`).
//!
//! Zero external HTTP client dependencies: all outbound traffic flows through
//! `xlite`'s native HTTP/2 and HTTP/1.1 connection pools (`Upstream`),
//! `StallGuard`, and `priority + weight` channel failover.

use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::store::Channel;

/// Parsed and validated Anthropic `/v1/messages` request ready to be rendered
/// as an OpenAI `/v1/chat/completions` payload for each candidate channel.
#[derive(Clone, Debug)]
pub struct ParsedAnthropicRequest {
    pub model: String,
    pub stream: bool,
    pub openai_messages: Vec<Value>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop: Option<Value>,
    pub tools: Option<Value>,
    pub tool_choice: Option<Value>,
    pub thinking: Option<Value>,
    pub extra: Map<String, Value>,
}

/// Parse an Anthropic `/v1/messages` JSON payload into a normalized
/// [`ParsedAnthropicRequest`].
pub fn parse_anthropic_request(payload: &Value) -> Result<ParsedAnthropicRequest, String> {
    let Some(obj) = payload.as_object() else {
        return Err("request body must be a JSON object".to_string());
    };
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "missing model".to_string())?
        .to_string();

    let raw_messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?;

    let mut openai_messages = Vec::new();
    if let Some(sys) = obj.get("system") {
        let sys_text = extract_system_text(sys);
        if !sys_text.is_empty() {
            openai_messages.push(json!({
                "role": "system",
                "content": sys_text,
            }));
        }
    }

    for msg in raw_messages {
        append_anthropic_message_as_openai(&mut openai_messages, msg)?;
    }

    let tools = obj.get("tools").and_then(anthropic_tools_to_openai_tools);
    let tool_choice = match obj.get("tool_choice") {
        Some(tc) => anthropic_tool_choice_to_openai_tool_choice(tc)?,
        None => None,
    };

    let mut extra = Map::new();
    for (k, v) in obj {
        if !matches!(
            k.as_str(),
            "model"
                | "messages"
                | "system"
                | "max_tokens"
                | "temperature"
                | "top_p"
                | "top_k"
                | "stop_sequences"
                | "stream"
                | "tools"
                | "tool_choice"
                | "thinking"
                | "metadata"
        ) && !k.starts_with('_')
            && !k.starts_with("pararouter_")
            && !k.starts_with("unigateway.")
        {
            extra.insert(k.clone(), v.clone());
        }
    }

    Ok(ParsedAnthropicRequest {
        model,
        stream: obj.get("stream").and_then(Value::as_bool).unwrap_or(false),
        openai_messages,
        max_tokens: obj.get("max_tokens").and_then(Value::as_u64),
        temperature: obj.get("temperature").and_then(Value::as_f64),
        top_p: obj.get("top_p").and_then(Value::as_f64),
        top_k: obj.get("top_k").and_then(Value::as_u64),
        stop: obj.get("stop_sequences").cloned(),
        tools,
        tool_choice,
        thinking: obj.get("thinking").cloned(),
        extra,
    })
}

fn extract_system_text(sys: &Value) -> String {
    match sys {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    Some(s.to_string())
                } else if item.get("type").and_then(Value::as_str) == Some("text") {
                    item.get("text").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn append_anthropic_message_as_openai(out: &mut Vec<Value>, msg: &Value) -> Result<(), String> {
    let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
    let content = msg
        .get("content")
        .ok_or_else(|| "message.content is required".to_string())?;

    if let Some(text) = content.as_str() {
        out.push(json!({
            "role": role,
            "content": text,
        }));
        return Ok(());
    }

    let Some(blocks) = content.as_array() else {
        return Err("message.content must be a string or array".to_string());
    };

    let mut content_parts: Vec<Value> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<(String, Value)> = Vec::new();

    for block in blocks {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("text");
        match block_type {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    content_parts.push(json!({
                        "type": "text",
                        "text": t,
                    }));
                }
            }
            "image" => {
                if let Some(source) = block.get("source")
                    && let Some(img_part) = anthropic_image_to_openai_part(source)
                {
                    content_parts.push(img_part);
                }
            }
            "thinking" => {
                if let Some(t) = block.get("thinking").and_then(Value::as_str)
                    && !t.is_empty()
                {
                    thinking_parts.push(t.to_string());
                }
            }
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or("call_0");
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let arguments = if let Some(s) = input.as_str() {
                    s.to_string()
                } else {
                    serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                };
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                }));
            }
            "tool_result" => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_0")
                    .to_string();
                let res_content = match block.get("content") {
                    Some(Value::String(s)) => Value::String(s.clone()),
                    Some(Value::Array(arr)) => {
                        let joined = arr
                            .iter()
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n");
                        Value::String(joined)
                    }
                    Some(other) => Value::String(other.to_string()),
                    None => Value::String(String::new()),
                };
                tool_results.push((tool_use_id, res_content));
            }
            _ => {}
        }
    }

    if !content_parts.is_empty() || !tool_calls.is_empty() || !thinking_parts.is_empty() {
        let mut msg_obj = Map::new();
        msg_obj.insert("role".to_string(), Value::String(role.to_string()));
        let content_val = match content_parts.as_slice() {
            [] => Value::String(String::new()),
            [single] if single.get("type").and_then(Value::as_str) == Some("text") => single
                .get("text")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new())),
            _ => Value::Array(content_parts),
        };
        msg_obj.insert("content".to_string(), content_val);
        if !thinking_parts.is_empty() {
            msg_obj.insert(
                "reasoning_content".to_string(),
                Value::String(thinking_parts.join("\n")),
            );
        }
        if !tool_calls.is_empty() {
            msg_obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
        }
        out.push(Value::Object(msg_obj));
    }

    for (tool_call_id, res_content) in tool_results {
        out.push(json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": res_content,
        }));
    }

    Ok(())
}

fn anthropic_image_to_openai_part(source: &Value) -> Option<Value> {
    match source.get("type").and_then(Value::as_str) {
        Some("url") => {
            let url = source.get("url").and_then(Value::as_str)?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": url }
            }))
        }
        Some("base64") => {
            let media_type = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") }
            }))
        }
        _ => None,
    }
}

fn anthropic_tools_to_openai_tools(tools: &Value) -> Option<Value> {
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let converted: Vec<Value> = arr
        .iter()
        .map(|tool| {
            if tool.get("type").and_then(Value::as_str) == Some("function")
                && tool.get("function").is_some()
            {
                return tool.clone();
            }
            let name = tool.get("name").and_then(Value::as_str).unwrap_or_default();
            let mut fn_obj = Map::new();
            fn_obj.insert("name".to_string(), Value::String(name.to_string()));
            if let Some(desc) = tool.get("description") {
                fn_obj.insert("description".to_string(), desc.clone());
            }
            let params = tool
                .get("input_schema")
                .or_else(|| tool.get("parameters"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            fn_obj.insert("parameters".to_string(), params);
            json!({
                "type": "function",
                "function": Value::Object(fn_obj),
            })
        })
        .collect();
    Some(Value::Array(converted))
}

fn anthropic_tool_choice_to_openai_tool_choice(tc: &Value) -> Result<Option<Value>, String> {
    if let Some(s) = tc.as_str() {
        return Ok(Some(Value::String(s.to_string())));
    }
    let Some(obj) = tc.as_object() else {
        return Ok(None);
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(Some(Value::String("auto".to_string()))),
        Some("any" | "required") => Ok(Some(Value::String("required".to_string()))),
        Some("none") => Ok(Some(Value::String("none".to_string()))),
        Some("tool") => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| "tool_choice of type `tool` requires `name`".to_string())?;
            Ok(Some(json!({
                "type": "function",
                "function": { "name": name }
            })))
        }
        Some("function") => Ok(Some(tc.clone())),
        _ => Ok(None),
    }
}

/// Build an OpenAI `/v1/chat/completions` JSON request body for `channel` from
/// a [`ParsedAnthropicRequest`].
pub fn build_openai_chat_body_from_anthropic(
    channel: &Channel,
    request: &ParsedAnthropicRequest,
) -> Result<Bytes, String> {
    let mut payload = Map::new();
    payload.insert(
        "model".to_string(),
        Value::String(channel.upstream_model.clone()),
    );
    payload.insert(
        "messages".to_string(),
        Value::Array(request.openai_messages.clone()),
    );
    payload.insert("stream".to_string(), Value::Bool(request.stream));

    if let Some(max_tokens) = request.max_tokens {
        payload.insert("max_tokens".to_string(), json!(max_tokens));
    }
    if let Some(temp) = request.temperature {
        payload.insert("temperature".to_string(), json!(temp));
    }
    if let Some(top_p) = request.top_p {
        payload.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(top_k) = request.top_k {
        payload.insert("top_k".to_string(), json!(top_k));
    }
    if let Some(stop) = &request.stop {
        payload.insert("stop".to_string(), stop.clone());
    }
    if let Some(tools) = &request.tools {
        payload.insert("tools".to_string(), tools.clone());
        if let Some(tc) = &request.tool_choice {
            payload.insert("tool_choice".to_string(), tc.clone());
        }
    }
    if let Some(thinking) = &request.thinking
        && channel
            .upstream_model
            .to_ascii_lowercase()
            .contains("claude")
    {
        payload.insert("thinking".to_string(), thinking.clone());
    }
    for (k, v) in &request.extra {
        payload.entry(k.clone()).or_insert_with(|| v.clone());
    }
    if request.stream {
        payload
            .entry("stream_options".to_string())
            .or_insert_with(|| json!({ "include_usage": true }));
    }

    serde_json::to_vec(&Value::Object(payload))
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}

/// Translate a non-streaming OpenAI `/v1/chat/completions` JSON response body
/// into an Anthropic `/v1/messages` JSON object.
pub fn translate_openai_chat_to_anthropic_json(
    body: &[u8],
    requested_model: &str,
) -> Result<Value, String> {
    let raw: Value = serde_json::from_slice(body)
        .map_err(|e| format!("failed to parse upstream chat response: {e}"))?;

    if raw.get("type").and_then(Value::as_str) == Some("message") {
        return Ok(raw);
    }

    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .map(|id| {
            if id.starts_with("msg_") {
                id.to_string()
            } else {
                format!("msg_{id}")
            }
        })
        .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4().simple()));

    let choice = raw
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let mut content_blocks: Vec<Value> = Vec::new();
    let mut has_tool_use = false;

    if let Some(message) = choice.and_then(|c| c.get("message")) {
        if let Some(reasoning) = message
            .get("reasoning_content")
            .or_else(|| message.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": reasoning,
                "signature": "",
            }));
        }

        if let Some(content) = message.get("content") {
            match content {
                Value::String(s) => {
                    append_text_with_optional_think_tags(&mut content_blocks, s);
                }
                Value::Array(parts) => {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            append_text_with_optional_think_tags(&mut content_blocks, t);
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for (idx, tc) in tool_calls.iter().enumerate() {
                has_tool_use = true;
                let tc_id = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_{idx}"));
                let name = tc
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let args_raw = tc
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let input = serde_json::from_str::<Value>(args_raw)
                    .unwrap_or_else(|_| json!({ "raw": args_raw }));
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": tc_id,
                    "name": name,
                    "input": input,
                }));
            }
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(json!({
            "type": "text",
            "text": "",
        }));
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or(if has_tool_use { "tool_calls" } else { "stop" });
    let stop_reason = map_openai_finish_reason_to_anthropic(finish_reason);

    let (input_tokens, output_tokens) = extract_token_counts(raw.get("usage"));

    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": requested_model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
}

fn append_text_with_optional_think_tags(blocks: &mut Vec<Value>, text: &str) {
    if let Some(rest) = text.trim_start().strip_prefix("<think>")
        && let Some((thinking, after)) = rest.split_once("</think>")
    {
        if !thinking.trim().is_empty() {
            blocks.push(json!({
                "type": "thinking",
                "thinking": thinking.trim(),
                "signature": "",
            }));
        }
        let trimmed_after = after.trim_start_matches(['\r', '\n']);
        if !trimmed_after.is_empty() {
            blocks.push(json!({
                "type": "text",
                "text": trimmed_after,
            }));
        }
        return;
    }
    if !text.is_empty() {
        blocks.push(json!({
            "type": "text",
            "text": text,
        }));
    }
}

fn map_openai_finish_reason_to_anthropic(reason: &str) -> &'static str {
    match reason {
        "stop" | "eos" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" | "function_call" | "tool_use" => "tool_use",
        _ => "end_turn",
    }
}

fn extract_token_counts(usage: Option<&Value>) -> (u64, u64) {
    let Some(u) = usage else {
        return (0, 0);
    };
    let input = u
        .get("prompt_tokens")
        .or_else(|| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = u
        .get("completion_tokens")
        .or_else(|| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (input, output)
}

#[derive(Default)]
struct PendingToolCall {
    id: String,
    name: String,
    content_index: Option<usize>,
    buffered_args: String,
    closed: bool,
}

/// Translate an upstream OpenAI `/v1/chat/completions` SSE byte stream into an
/// Anthropic `/v1/messages` SSE byte stream.
pub fn translate_openai_sse_to_anthropic_stream<S>(
    upstream_stream: S,
    requested_model: String,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    tokio::spawn(async move {
        let mut upstream = upstream_stream;
        let mut buffer = Vec::new();
        let message_id = format!("msg_{}", Uuid::new_v4().simple());
        let mut prelude_sent = false;
        let mut next_block_index = 0usize;
        let mut active_block_index: Option<usize> = None;
        let mut active_block_kind: Option<&'static str> = None;
        let mut pending_tools: BTreeMap<usize, PendingToolCall> = BTreeMap::new();
        let mut stop_reason: Option<&'static str> = None;
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut done = false;

        let ensure_prelude = |tx: &mpsc::Sender<Result<Bytes, io::Error>>,
                              prelude_sent: &mut bool,
                              message_id: &str,
                              model: &str| {
            if *prelude_sent {
                return Vec::new();
            }
            *prelude_sent = true;
            let start = json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 }
                }
            });
            let ping = json!({ "type": "ping" });
            let _ = tx;
            vec![sse_frame("message_start", &start), sse_frame("ping", &ping)]
        };

        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    for data in drain_sse_data_frames(&mut buffer) {
                        if data == "[DONE]" {
                            done = true;
                            break;
                        }
                        let Ok(raw) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        if is_native_anthropic_sse_event(&raw) {
                            let ev_type = raw
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("message_delta");
                            if tx.send(Ok(sse_frame(ev_type, &raw))).await.is_err() {
                                return;
                            }
                            prelude_sent = true;
                            continue;
                        }

                        for frame in
                            ensure_prelude(&tx, &mut prelude_sent, &message_id, &requested_model)
                        {
                            if tx.send(Ok(frame)).await.is_err() {
                                return;
                            }
                        }

                        if let Some(usage) = raw.get("usage") {
                            let (inp, out) = extract_token_counts(Some(usage));
                            if inp > 0 || out > 0 {
                                input_tokens = inp;
                                output_tokens = out;
                            }
                        }

                        let Some(choice) = raw
                            .get("choices")
                            .and_then(Value::as_array)
                            .and_then(|c| c.first())
                        else {
                            continue;
                        };

                        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                            stop_reason = Some(map_openai_finish_reason_to_anthropic(fr));
                        }

                        let Some(delta) = choice.get("delta") else {
                            continue;
                        };

                        if let Some(reasoning) = delta
                            .get("reasoning_content")
                            .or_else(|| delta.get("reasoning"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            let idx = open_or_reuse_block(
                                &tx,
                                &mut next_block_index,
                                &mut active_block_index,
                                &mut active_block_kind,
                                "thinking",
                            )
                            .await;
                            let Some(idx) = idx else { return };
                            let ev = json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": {
                                    "type": "thinking_delta",
                                    "thinking": reasoning,
                                }
                            });
                            if tx
                                .send(Ok(sse_frame("content_block_delta", &ev)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }

                        if let Some(text) = delta
                            .get("content")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            let idx = open_or_reuse_block(
                                &tx,
                                &mut next_block_index,
                                &mut active_block_index,
                                &mut active_block_kind,
                                "text",
                            )
                            .await;
                            let Some(idx) = idx else { return };
                            let ev = json!({
                                "type": "content_block_delta",
                                "index": idx,
                                "delta": {
                                    "type": "text_delta",
                                    "text": text,
                                }
                            });
                            if tx
                                .send(Ok(sse_frame("content_block_delta", &ev)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }

                        if let Some(tc_arr) = delta.get("tool_calls").and_then(Value::as_array) {
                            if close_active_block(
                                &tx,
                                &mut active_block_index,
                                &mut active_block_kind,
                            )
                            .await
                            .is_err()
                            {
                                return;
                            }
                            for (pos, tc_delta) in tc_arr.iter().enumerate() {
                                let slot = tc_delta
                                    .get("index")
                                    .and_then(Value::as_u64)
                                    .map(|i| i as usize)
                                    .unwrap_or(pos);
                                let entry = pending_tools.entry(slot).or_default();
                                if let Some(id) = tc_delta
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                {
                                    entry.id = id.to_string();
                                }
                                if let Some(name) = tc_delta
                                    .pointer("/function/name")
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                {
                                    entry.name.push_str(name);
                                }
                                let args_piece = tc_delta
                                    .pointer("/function/arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();

                                if entry.content_index.is_none() && !entry.name.is_empty() {
                                    let idx = next_block_index;
                                    next_block_index += 1;
                                    entry.content_index = Some(idx);
                                    if entry.id.is_empty() {
                                        entry.id = format!("toolu_{idx}");
                                    }
                                    let start_ev = json!({
                                        "type": "content_block_start",
                                        "index": idx,
                                        "content_block": {
                                            "type": "tool_use",
                                            "id": entry.id,
                                            "name": entry.name,
                                            "input": {}
                                        }
                                    });
                                    if tx
                                        .send(Ok(sse_frame("content_block_start", &start_ev)))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    if !entry.buffered_args.is_empty() {
                                        let buffered = std::mem::take(&mut entry.buffered_args);
                                        let delta_ev = json!({
                                            "type": "content_block_delta",
                                            "index": idx,
                                            "delta": {
                                                "type": "input_json_delta",
                                                "partial_json": buffered,
                                            }
                                        });
                                        if tx
                                            .send(Ok(sse_frame("content_block_delta", &delta_ev)))
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }

                                if !args_piece.is_empty() {
                                    if let Some(idx) = entry.content_index {
                                        let delta_ev = json!({
                                            "type": "content_block_delta",
                                            "index": idx,
                                            "delta": {
                                                "type": "input_json_delta",
                                                "partial_json": args_piece,
                                            }
                                        });
                                        if tx
                                            .send(Ok(sse_frame("content_block_delta", &delta_ev)))
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                    } else {
                                        entry.buffered_args.push_str(args_piece);
                                    }
                                }
                            }
                        }
                    }
                    if done {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            }
        }

        for frame in ensure_prelude(&tx, &mut prelude_sent, &message_id, &requested_model) {
            if tx.send(Ok(frame)).await.is_err() {
                return;
            }
        }

        let _ = close_active_block(&tx, &mut active_block_index, &mut active_block_kind).await;

        for entry in pending_tools.values_mut() {
            if let Some(idx) = entry.content_index
                && !entry.closed
            {
                entry.closed = true;
                let stop_ev = json!({
                    "type": "content_block_stop",
                    "index": idx,
                });
                let _ = tx.send(Ok(sse_frame("content_block_stop", &stop_ev))).await;
            }
        }

        let final_stop = stop_reason.unwrap_or(if pending_tools.is_empty() {
            "end_turn"
        } else {
            "tool_use"
        });
        let msg_delta = json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": final_stop,
                "stop_sequence": null,
            },
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            }
        });
        let _ = tx.send(Ok(sse_frame("message_delta", &msg_delta))).await;
        let msg_stop = json!({ "type": "message_stop" });
        let _ = tx.send(Ok(sse_frame("message_stop", &msg_stop))).await;
    });

    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

async fn open_or_reuse_block(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    next_block_index: &mut usize,
    active_block_index: &mut Option<usize>,
    active_block_kind: &mut Option<&'static str>,
    kind: &'static str,
) -> Option<usize> {
    if let Some(idx) = *active_block_index
        && *active_block_kind == Some(kind)
    {
        return Some(idx);
    }
    if close_active_block(tx, active_block_index, active_block_kind)
        .await
        .is_err()
    {
        return None;
    }
    let idx = *next_block_index;
    *next_block_index += 1;
    let block = if kind == "thinking" {
        json!({ "type": "thinking", "thinking": "", "signature": "" })
    } else {
        json!({ "type": "text", "text": "" })
    };
    let ev = json!({
        "type": "content_block_start",
        "index": idx,
        "content_block": block,
    });
    if tx
        .send(Ok(sse_frame("content_block_start", &ev)))
        .await
        .is_err()
    {
        return None;
    }
    *active_block_index = Some(idx);
    *active_block_kind = Some(kind);
    Some(idx)
}

async fn close_active_block(
    tx: &mpsc::Sender<Result<Bytes, io::Error>>,
    active_block_index: &mut Option<usize>,
    active_block_kind: &mut Option<&'static str>,
) -> Result<(), ()> {
    let Some(idx) = active_block_index.take() else {
        return Ok(());
    };
    let _ = active_block_kind.take();
    let ev = json!({
        "type": "content_block_stop",
        "index": idx,
    });
    tx.send(Ok(sse_frame("content_block_stop", &ev)))
        .await
        .map_err(|_| ())
}

fn is_native_anthropic_sse_event(raw: &Value) -> bool {
    matches!(
        raw.get("type").and_then(Value::as_str),
        Some(
            "message_start"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
                | "ping"
        )
    )
}

fn sse_frame(event: &str, data: &Value) -> Bytes {
    let serialized = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    Bytes::from(format!("event: {event}\ndata: {serialized}\n\n"))
}

/// Parsed and normalized OpenAI `/v1/responses` request.
#[derive(Clone, Debug)]
pub struct ParsedResponsesRequest {
    pub model: String,
    pub stream: bool,
    pub payload: Map<String, Value>,
}

/// Parse and normalize an incoming OpenAI `/v1/responses` JSON payload.
pub fn parse_responses_request(payload: &Value) -> Result<ParsedResponsesRequest, String> {
    let Some(obj) = payload.as_object() else {
        return Err("request body must be a JSON object".to_string());
    };
    let model = obj
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| "missing model".to_string())?
        .to_string();
    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut clean = Map::new();
    for (k, v) in obj {
        if !k.starts_with('_') && !k.starts_with("pararouter_") && !k.starts_with("unigateway.") {
            clean.insert(k.clone(), v.clone());
        }
    }
    clean.insert("model".to_string(), Value::String(model.clone()));
    clean.insert("stream".to_string(), Value::Bool(stream));

    Ok(ParsedResponsesRequest {
        model,
        stream,
        payload: clean,
    })
}

/// Build an outbound `/v1/responses` JSON payload for `channel`.
pub fn build_openai_responses_body(
    channel: &Channel,
    request: &ParsedResponsesRequest,
) -> Result<Bytes, String> {
    let mut out = request.payload.clone();
    out.insert(
        "model".to_string(),
        Value::String(channel.upstream_model.clone()),
    );
    serde_json::to_vec(&Value::Object(out))
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}

/// Convert a [`ParsedResponsesRequest`] into an OpenAI `/v1/chat/completions`
/// request body for channels whose upstream only implements Chat Completions.
pub fn build_openai_chat_body_from_responses(
    channel: &Channel,
    request: &ParsedResponsesRequest,
) -> Result<Bytes, String> {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instructions) = request
        .payload
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        messages.push(json!({
            "role": "system",
            "content": instructions,
        }));
    }

    match request.payload.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({
                "role": "user",
                "content": text,
            }));
        }
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    messages.push(json!({
                        "role": "user",
                        "content": s,
                    }));
                    continue;
                }
                let item_type = item.get("type").and_then(Value::as_str);
                if item_type == Some("function_call_output") {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_0");
                    let output = item
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": output,
                    }));
                    continue;
                }
                if item_type == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("call_0");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    let arguments = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("{}");
                    messages.push(json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments,
                            }
                        }]
                    }));
                    continue;
                }

                let role = match item.get("role").and_then(Value::as_str) {
                    Some("system" | "developer") => "system",
                    Some("assistant") => "assistant",
                    Some("tool") => "tool",
                    _ => "user",
                };
                if let Some(text) = item.get("content").and_then(Value::as_str) {
                    messages.push(json!({
                        "role": role,
                        "content": text,
                    }));
                } else if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    let text_joined = parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    messages.push(json!({
                        "role": role,
                        "content": text_joined,
                    }));
                }
            }
        }
        _ => {}
    }

    if messages.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": "",
        }));
    }

    let mut chat = Map::new();
    chat.insert(
        "model".to_string(),
        Value::String(channel.upstream_model.clone()),
    );
    chat.insert("messages".to_string(), Value::Array(messages));
    chat.insert("stream".to_string(), Value::Bool(request.stream));

    if let Some(temp) = request.payload.get("temperature") {
        chat.insert("temperature".to_string(), temp.clone());
    }
    if let Some(top_p) = request.payload.get("top_p") {
        chat.insert("top_p".to_string(), top_p.clone());
    }
    if let Some(max_out) = request.payload.get("max_output_tokens") {
        chat.insert("max_tokens".to_string(), max_out.clone());
    }
    if let Some(tools) = request.payload.get("tools") {
        chat.insert(
            "tools".to_string(),
            normalize_responses_tools_for_chat(tools.clone()),
        );
    }
    if let Some(tc) = request.payload.get("tool_choice") {
        chat.insert("tool_choice".to_string(), tc.clone());
    }
    if let Some(effort) = request
        .payload
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        chat.insert(
            "reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    if request.stream {
        chat.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    serde_json::to_vec(&Value::Object(chat))
        .map(Bytes::from)
        .map_err(|e| e.to_string())
}

fn normalize_responses_tools_for_chat(tools: Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools;
    };
    let mapped: Vec<Value> = arr
        .iter()
        .map(|tool| {
            if tool.get("type").and_then(Value::as_str) == Some("function")
                && tool.get("function").is_none()
                && let Some(name) = tool.get("name").and_then(Value::as_str)
            {
                let mut fn_obj = Map::new();
                fn_obj.insert("name".to_string(), Value::String(name.to_string()));
                if let Some(desc) = tool.get("description") {
                    fn_obj.insert("description".to_string(), desc.clone());
                }
                if let Some(params) = tool.get("parameters") {
                    fn_obj.insert("parameters".to_string(), params.clone());
                }
                json!({
                    "type": "function",
                    "function": Value::Object(fn_obj),
                })
            } else {
                tool.clone()
            }
        })
        .collect();
    Value::Array(mapped)
}

/// Translate a non-streaming OpenAI `/v1/chat/completions` JSON response body
/// into an OpenAI `/v1/responses` JSON object.
pub fn translate_openai_chat_to_responses_json(
    body: &[u8],
    requested_model: &str,
) -> Result<Value, String> {
    let raw: Value = serde_json::from_slice(body)
        .map_err(|e| format!("failed to parse upstream chat response: {e}"))?;
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .map(|id| {
            if id.starts_with("resp_") {
                id.to_string()
            } else {
                format!("resp_{id}")
            }
        })
        .unwrap_or_else(|| format!("resp_{}", Uuid::new_v4().simple()));

    let choice = raw
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let message = choice.and_then(|c| c.get("message"));
    let text = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let msg_id = format!("msg_{}", Uuid::new_v4().simple());
    let mut output_items = vec![json!({
        "id": msg_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [
            {
                "type": "output_text",
                "text": text,
                "annotations": []
            }
        ]
    })];

    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for tc in tool_calls {
            let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("call_0");
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = tc
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            output_items.push(json!({
                "type": "function_call",
                "id": format!("fc_{call_id}"),
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed"
            }));
        }
    }

    let (input_tokens, output_tokens) = extract_token_counts(raw.get("usage"));

    Ok(json!({
        "id": id,
        "object": "response",
        "status": "completed",
        "model": requested_model,
        "output": output_items,
        "output_text": text,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    }))
}

/// Translate an OpenAI `/v1/chat/completions` SSE byte stream into an OpenAI
/// `/v1/responses` SSE byte stream (`response.created`,
/// `response.output_text.delta`, `response.completed`).
pub fn translate_openai_sse_to_responses_stream<S>(
    upstream_stream: S,
    requested_model: String,
) -> Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<Bytes, io::Error>>(32);
    tokio::spawn(async move {
        let mut upstream = upstream_stream;
        let mut buffer = Vec::new();
        let response_id = format!("resp_{}", Uuid::new_v4().simple());
        let mut created_sent = false;
        let mut full_text = String::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut done = false;

        let make_created = |response_id: &str, model: &str| {
            let created = json!({
                "type": "response.created",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "model": model,
                    "status": "in_progress"
                }
            });
            sse_frame("response.created", &created)
        };

        while let Some(item) = upstream.next().await {
            match item {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    for data in drain_sse_data_frames(&mut buffer) {
                        if data == "[DONE]" {
                            done = true;
                            break;
                        }
                        let Ok(raw) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        if !created_sent {
                            created_sent = true;
                            if tx
                                .send(Ok(make_created(&response_id, &requested_model)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        if let Some(usage) = raw.get("usage") {
                            let (inp, out) = extract_token_counts(Some(usage));
                            if inp > 0 || out > 0 {
                                input_tokens = inp;
                                output_tokens = out;
                            }
                        }
                        if let Some(delta) = raw
                            .get("choices")
                            .and_then(Value::as_array)
                            .and_then(|c| c.first())
                            .and_then(|c| c.get("delta"))
                            .and_then(|d| d.get("content"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            full_text.push_str(delta);
                            let ev = json!({
                                "type": "response.output_text.delta",
                                "response_id": response_id,
                                "delta": delta,
                            });
                            if tx
                                .send(Ok(sse_frame("response.output_text.delta", &ev)))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    if done {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    return;
                }
            }
        }

        if !created_sent
            && tx
                .send(Ok(make_created(&response_id, &requested_model)))
                .await
                .is_err()
        {
            return;
        }
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "object": "response",
                "model": requested_model,
                "status": "completed",
                "output_text": full_text,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "total_tokens": input_tokens + output_tokens,
                }
            }
        });
        let _ = tx
            .send(Ok(sse_frame("response.completed", &completed)))
            .await;
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });

    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Normalize an upstream error payload into Anthropic's
/// `{"type":"error","error":{"type":...,"message":...}}` envelope when serving
/// `/v1/messages`.
pub fn format_anthropic_error_body(status: u16, raw_body: &[u8], fallback_msg: &str) -> Bytes {
    let default_type = match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        402 | 403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "api_error",
    };
    if let Ok(val) = serde_json::from_slice::<Value>(raw_body) {
        if val.get("type").and_then(Value::as_str) == Some("error") && val.get("error").is_some() {
            return Bytes::from(raw_body.to_vec());
        }
        if let Some(err_obj) = val.get("error") {
            if let Some(msg) = err_obj.get("message").and_then(Value::as_str) {
                let err_type = err_obj
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or(default_type);
                return Bytes::from(
                    serde_json::to_vec(&json!({
                        "type": "error",
                        "error": {
                            "type": err_type,
                            "message": msg,
                        }
                    }))
                    .unwrap_or_default(),
                );
            }
            if let Some(msg) = err_obj.as_str() {
                return Bytes::from(
                    serde_json::to_vec(&json!({
                        "type": "error",
                        "error": {
                            "type": default_type,
                            "message": msg,
                        }
                    }))
                    .unwrap_or_default(),
                );
            }
        }
    }
    let msg = std::str::from_utf8(raw_body)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_msg);
    Bytes::from(
        serde_json::to_vec(&json!({
            "type": "error",
            "error": {
                "type": default_type,
                "message": msg,
            }
        }))
        .unwrap_or_default(),
    )
}

fn drain_sse_data_frames(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some((idx, delim_len)) = find_sse_delimiter(buffer) {
        let frame_bytes: Vec<u8> = buffer.drain(..idx).collect();
        buffer.drain(..delim_len);
        if let Some(data) = parse_sse_data(&frame_bytes) {
            out.push(data);
        }
    }
    out
}

fn parse_sse_data(frame_bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(frame_bytes);
    let mut data_lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(val) = line.strip_prefix("data:") {
            data_lines.push(val.trim_start().to_string());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    if buffer.len() < 2 {
        return None;
    }
    for i in 0..(buffer.len() - 1) {
        if buffer[i] == b'\n' && buffer[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buffer.len()
            && buffer[i] == b'\r'
            && buffer[i + 1] == b'\n'
            && buffer[i + 2] == b'\r'
            && buffer[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
    }
    None
}

/// Collect a bounded response body from an upstream stream.
pub async fn collect_upstream_body<S>(mut stream: S, max_bytes: usize) -> Result<Bytes, String>
where
    S: Stream<Item = Result<Bytes, io::Error>> + Unpin,
{
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        if buf.len() + bytes.len() > max_bytes {
            return Err(format!("upstream response body exceeded {max_bytes} bytes"));
        }
        buf.extend_from_slice(&bytes);
    }
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_request_translates_system_tools_and_tool_results() {
        let req_json = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "system": [{"type": "text", "text": "You are helpful."}],
            "messages": [
                {"role": "user", "content": "What is the weather in HK?"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "Need to call get_weather"},
                        {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "HK"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "26C sunny"}
                    ]
                }
            ],
            "tools": [
                {
                    "name": "get_weather",
                    "description": "Weather lookup",
                    "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            ],
            "tool_choice": {"type": "tool", "name": "get_weather"}
        });

        let parsed = parse_anthropic_request(&req_json).expect("valid anthropic request");
        assert_eq!(parsed.model, "claude-sonnet-4-6");
        assert_eq!(parsed.openai_messages.len(), 4);
        assert_eq!(parsed.openai_messages[0]["role"], "system");
        assert_eq!(parsed.openai_messages[0]["content"], "You are helpful.");
        assert_eq!(parsed.openai_messages[2]["role"], "assistant");
        assert_eq!(
            parsed.openai_messages[2]["reasoning_content"],
            "Need to call get_weather"
        );
        assert_eq!(
            parsed.openai_messages[2]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        assert_eq!(parsed.openai_messages[3]["role"], "tool");
        assert_eq!(parsed.openai_messages[3]["tool_call_id"], "toolu_1");
        assert_eq!(parsed.openai_messages[3]["content"], "26C sunny");
        assert_eq!(
            parsed.tool_choice,
            Some(json!({"type": "function", "function": {"name": "get_weather"}}))
        );
    }

    #[test]
    fn openai_chat_response_translates_to_anthropic_message_with_thinking_and_tools() {
        let openai_resp = br#"{
            "id": "chatcmpl-123",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "reasoning_content": "Let me check",
                    "content": "Calling tool",
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"HK\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}
        }"#;

        let msg = translate_openai_chat_to_anthropic_json(openai_resp, "claude-sonnet-4-6")
            .expect("translate");
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["model"], "claude-sonnet-4-6");
        assert_eq!(msg["stop_reason"], "tool_use");
        assert_eq!(msg["usage"]["input_tokens"], 12);
        assert_eq!(msg["usage"]["output_tokens"], 8);
        let blocks = msg["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "Let me check");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "Calling tool");
        assert_eq!(blocks[2]["type"], "tool_use");
        assert_eq!(blocks[2]["name"], "get_weather");
        assert_eq!(blocks[2]["input"]["city"], "HK");
    }
}
