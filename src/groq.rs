use eframe::egui;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const GROQ_CHAT_URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const PREEMPTIVE_TOKEN_THRESHOLD: u64 = 2500;
const PREEMPTIVE_COOLDOWN_SECS: u64 = 45;
const STAGE1_OCR_MAX_TOKENS: u32 = 1200;
const STAGE2_SOLVE_MAX_TOKENS: u32 = 2560;

#[derive(Debug, Clone)]
pub struct RequestMetrics {
    pub capture_ms: u64,
    pub image_kb: usize,
    pub ttft_ms: u64,
    pub total_ms: u64,
    pub token_count: usize,
    pub tokens_per_sec: f64,
    pub remaining_tokens: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub enum GroqEvent {
    Token(String),
    Status(String),
    KeyRotated {
        from_index: usize,
        to_index: usize,
        reason: String,
    },
    UserTurn(serde_json::Value),
    Done(Option<RequestMetrics>),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct KeyPool {
    keys: Vec<String>,
    current_index: Arc<AtomicUsize>,
    cooldowns: Arc<Mutex<HashMap<usize, Instant>>>,
}

impl KeyPool {
    pub fn new(mut keys: Vec<String>) -> Self {
        keys.retain(|k| !k.trim().is_empty());
        Self {
            keys,
            current_index: Arc::new(AtomicUsize::new(0)),
            cooldowns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn total_keys(&self) -> usize {
        self.keys.len()
    }

    pub fn current_key_index(&self) -> usize {
        if self.keys.is_empty() {
            return 0;
        }
        self.current_index.load(Ordering::SeqCst) % self.keys.len()
    }

    pub fn get_current_key(&self) -> Option<String> {
        self.get_current_indexed_key().map(|(_, k)| k)
    }

    /// Atomically resolves a key index and key, skipping cooled-down keys if an uncooled one exists.
    pub fn get_current_indexed_key(&self) -> Option<(usize, String)> {
        if self.keys.is_empty() {
            return None;
        }
        let total = self.keys.len();
        let idx = self.current_index.load(Ordering::SeqCst) % total;
        if !self.is_key_in_cooldown(idx) {
            return Some((idx, self.keys[idx].clone()));
        }
        for offset in 1..total {
            let candidate = (idx + offset) % total;
            if !self.is_key_in_cooldown(candidate) {
                self.current_index.store(candidate, Ordering::SeqCst);
                return Some((candidate, self.keys[candidate].clone()));
            }
        }
        Some((idx, self.keys[idx].clone()))
    }

    pub fn mark_cooldown(&self, idx: usize, seconds: u64) {
        let mut cooldowns = self.cooldowns.lock().unwrap();
        let until = Instant::now() + Duration::from_secs(seconds.max(1));
        cooldowns.insert(idx, until);
    }

    pub fn is_key_in_cooldown(&self, idx: usize) -> bool {
        let cooldowns = self.cooldowns.lock().unwrap();
        if let Some(&until) = cooldowns.get(&idx) {
            Instant::now() < until
        } else {
            false
        }
    }

    pub fn all_in_cooldown(&self) -> bool {
        let cooldowns = self.cooldowns.lock().unwrap();
        let now = Instant::now();
        !self.keys.is_empty()
            && self
                .keys
                .iter()
                .enumerate()
                .all(|(idx, _)| cooldowns.get(&idx).is_some_and(|until| *until > now))
    }

    pub fn soonest_cooldown_secs(&self) -> Option<u64> {
        let cooldowns = self.cooldowns.lock().unwrap();
        let now = Instant::now();
        cooldowns
            .values()
            .filter(|until| **until > now)
            .min()
            .map(|until| (*until - now).as_secs().max(1))
    }

    /// Rotates to the next available key that is not in cooldown.
    /// If all keys are in cooldown, picks the one that recovers soonest.
    pub fn rotate_to_next(&self) -> Option<(usize, usize, String)> {
        if self.keys.len() <= 1 {
            return None;
        }
        let total = self.keys.len();
        let old_idx = self.current_key_index();

        // 1. Pick the first next key that is not in cooldown
        for offset in 1..total {
            let candidate_idx = (old_idx + offset) % total;
            if !self.is_key_in_cooldown(candidate_idx) {
                self.current_index.store(candidate_idx, Ordering::SeqCst);
                return Some((
                    old_idx + 1,
                    candidate_idx + 1,
                    self.keys[candidate_idx].clone(),
                ));
            }
        }

        // 2. All keys are in cooldown: pick the candidate whose cooldown expires soonest
        let next_idx = {
            let cooldowns = self.cooldowns.lock().unwrap();
            (1..total)
                .map(|offset| (old_idx + offset) % total)
                .min_by_key(|idx| cooldowns.get(idx).copied().unwrap_or_else(Instant::now))
                .unwrap_or((old_idx + 1) % total)
        };
        self.current_index.store(next_idx, Ordering::SeqCst);
        Some((old_idx + 1, next_idx + 1, self.keys[next_idx].clone()))
    }
}

pub struct GroqClient {
    pool: Arc<KeyPool>,
    http: reqwest::Client,
    pub fast_model: String,
    pub vision_model: String,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
    reasoning: Option<String>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
    error: Option<serde_json::Value>,
}

enum LineOutcome {
    Continue,
    Done,
    Error(String),
}

impl GroqClient {
    pub fn new(pool: KeyPool) -> Self {
        Self::new_with_shared_pool(Arc::new(pool), reqwest::Client::new())
    }

    pub fn new_with_client(pool: KeyPool, http: reqwest::Client) -> Self {
        Self::new_with_shared_pool(Arc::new(pool), http)
    }

    pub fn new_with_shared_pool(pool: Arc<KeyPool>, http: reqwest::Client) -> Self {
        Self {
            pool,
            http,
            fast_model: "openai/gpt-oss-120b".to_string(),
            vision_model: "qwen/qwen3.8-27b".to_string(),
        }
    }

    pub fn pool(&self) -> &KeyPool {
        &self.pool
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn stream_request(
        &self,
        system_prompt: Option<String>,
        user_prompt: String,
        image_base64: Option<String>,
        capture_ms: u64,
        user_start: Instant,
        egui_ctx: egui::Context,
        tx: mpsc::Sender<GroqEvent>,
    ) {
        let image_kb = image_base64
            .as_ref()
            .map(|s| (s.len() * 3) / 4 / 1024)
            .unwrap_or(0);
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(json!({
                "role": "system",
                "content": sys
            }));
        }

        if let Some(img) = image_base64 {
            messages.push(json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": user_prompt },
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:image/jpeg;base64,{}", img)
                        }
                    }
                ]
            }));
        } else {
            messages.push(json!({
                "role": "user",
                "content": user_prompt
            }));
        }

        self.stream_messages_request(messages, capture_ms, image_kb, user_start, egui_ctx, tx)
            .await;
    }

    /// Sends a Groq chat completion payload with 429 key rotation and cooldown tracking.
    /// When every key is rate-limited, waits for the soonest reset and retries once.
    async fn post_with_retry(
        &self,
        payload: &serde_json::Value,
        tx: &mpsc::Sender<GroqEvent>,
        egui_ctx: &egui::Context,
    ) -> Result<(reqwest::Response, usize, Option<u64>), String> {
        let max_attempts = self.pool.total_keys().max(1);

        for round in 0..2 {
            let mut attempts = 0;

            while attempts < max_attempts {
                let (key_idx, current_key) = self
                    .pool
                    .get_current_indexed_key()
                    .ok_or_else(|| "No Groq API keys configured.".to_string())?;

                let res = self
                    .http
                    .post(GROQ_CHAT_URL)
                    .header("Authorization", format!("Bearer {}", current_key))
                    .header("Content-Type", "application/json")
                    .json(payload)
                    .send()
                    .await
                    .map_err(|e| format!("Network connection failed: {e}"))?;

                let status = res.status();
                let headers = res.headers().clone();

                let retry_after_secs = headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(5);

                let remaining_tokens = headers
                    .get("x-ratelimit-remaining-tokens")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok());

                if status.as_u16() == 429 {
                    attempts += 1;
                    self.pool.mark_cooldown(key_idx, retry_after_secs);

                    if self.pool.all_in_cooldown() {
                        break;
                    }

                    if let Some((from_i, to_i, _)) = self.pool.rotate_to_next() {
                        let _ = tx
                            .send(GroqEvent::KeyRotated {
                                from_index: from_i,
                                to_index: to_i,
                                reason: format!("429 Rate Limit (Retry after {retry_after_secs}s)"),
                            })
                            .await;
                        egui_ctx.request_repaint();
                        continue;
                    }
                    break;
                }

                if !status.is_success() {
                    let err_text = res
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unknown error".to_string());
                    return Err(format!("Groq API error ({status}): {err_text}"));
                }

                if let Some(rem_tok) = remaining_tokens {
                    if rem_tok < PREEMPTIVE_TOKEN_THRESHOLD {
                        self.pool.mark_cooldown(key_idx, PREEMPTIVE_COOLDOWN_SECS);
                        let _ = self.pool.rotate_to_next();
                    }
                }

                return Ok((res, key_idx, remaining_tokens));
            }

            if round == 1 {
                break;
            }

            let wait_secs = self.pool.soonest_cooldown_secs().unwrap_or(5).clamp(1, 60);

            for remaining in (1..=wait_secs).rev() {
                let _ = tx
                    .send(GroqEvent::Status(format!(
                        "All keys rate-limited. Retrying in {remaining}s..."
                    )))
                    .await;
                egui_ctx.request_repaint();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }

        Err(format!(
            "All {} configured Groq API keys are rate-limited. Wait a minute and try again.",
            self.pool.total_keys()
        ))
    }

    pub async fn stream_messages_request(
        &self,
        messages: Vec<serde_json::Value>,
        capture_ms: u64,
        image_kb: usize,
        user_start: Instant,
        egui_ctx: egui::Context,
        tx: mpsc::Sender<GroqEvent>,
    ) {
        if self.pool.is_empty() {
            let _ = tx
                .send(GroqEvent::Error("No Groq API keys configured.".to_string()))
                .await;
            egui_ctx.request_repaint();
            return;
        }

        let image_url_opt = find_last_image_url(&messages);
        let (final_model, final_messages, user_turn_to_record) = if let Some(img_url) =
            image_url_opt
        {
            let _ = tx
                .send(GroqEvent::Status(format!(
                    "Screen ({image_kb}KB) -> transcribing with Qwen OCR..."
                )))
                .await;
            egui_ctx.request_repaint();

            let ocr_payload = json!({
                "model": &self.vision_model,
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "text",
                                "text": "Transcribe ONLY the primary focused coding problem and code editor on screen verbatim. Ignore unrelated background windows. Do NOT solve, hint, critique, or explain.\n\
                                         Include:\n\
                                         1. Selected programming language or file extension (from the editor language dropdown, tab, or syntax).\n\
                                         2. Problem title, full description, examples, and constraints.\n\
                                         3. Current code and starter function/class signature in the editor.\n\
                                         4. Any visible test case input, actual output, expected output, or runtime error."
                            },
                            {
                                "type": "image_url",
                                "image_url": { "url": img_url }
                            }
                        ]
                    }
                ],
                "temperature": 0.1,
                "max_completion_tokens": STAGE1_OCR_MAX_TOKENS
            });

            let ocr_res = match self.post_with_retry(&ocr_payload, &tx, &egui_ctx).await {
                Ok((res, _, _)) => res,
                Err(err) => {
                    let _ = tx
                        .send(GroqEvent::Error(format!("OCR failed: {err}")))
                        .await;
                    egui_ctx.request_repaint();
                    return;
                }
            };

            let data = match ocr_res.json::<serde_json::Value>().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx
                        .send(GroqEvent::Error(format!("OCR response parse error: {e}")))
                        .await;
                    egui_ctx.request_repaint();
                    return;
                }
            };

            let ocr_text = data["choices"][0]["message"]["content"]
                .as_str()
                .map(str::trim)
                .unwrap_or("");
            if ocr_text.is_empty() {
                let _ = tx
                    .send(GroqEvent::Error(
                        "Qwen OCR returned empty transcription from screenshot.".to_string(),
                    ))
                    .await;
                egui_ctx.request_repaint();
                return;
            }

            let ocr_truncated = data["choices"][0]["finish_reason"].as_str() == Some("length");
            let status_msg = if ocr_truncated {
                "OCR truncated -> solving with GPT-OSS-120B (thinking)...".to_string()
            } else {
                "OCR done -> solving with GPT-OSS-120B (thinking)...".to_string()
            };
            let _ = tx.send(GroqEvent::Status(status_msg)).await;
            egui_ctx.request_repaint();

            let (staged_messages, staged_user_msg) = build_stage2_messages(&messages, ocr_text);
            (self.fast_model.clone(), staged_messages, staged_user_msg)
        } else {
            let last_user = messages
                .last()
                .cloned()
                .unwrap_or_else(|| json!({"role": "user", "content": ""}));
            (self.fast_model.clone(), messages, last_user)
        };

        let _ = tx.send(GroqEvent::UserTurn(user_turn_to_record)).await;

        let mut payload = json!({
            "model": final_model,
            "messages": final_messages,
            "stream": true,
            "temperature": 0.2,
            "max_completion_tokens": STAGE2_SOLVE_MAX_TOKENS
        });
        if final_model.starts_with("openai/gpt-oss") {
            payload["reasoning_effort"] = json!("low");
        }

        let (response, _key_idx, remaining_tokens) =
            match self.post_with_retry(&payload, &tx, &egui_ctx).await {
                Ok(ok) => ok,
                Err(err) => {
                    let _ = tx.send(GroqEvent::Error(err)).await;
                    egui_ctx.request_repaint();
                    return;
                }
            };

        let mut stream = response.bytes_stream();
        let mut raw_buf: Vec<u8> = Vec::new();
        let mut first_token_time: Option<Instant> = None;
        let mut token_count = 0usize;
        let mut reasoning_chunks = 0usize;
        let mut last_finish_reason: Option<String> = None;

        while let Some(chunk_res) = stream.next().await {
            match chunk_res {
                Ok(chunk) => {
                    raw_buf.extend_from_slice(&chunk);

                    while let Some(pos) = raw_buf.iter().position(|&b| b == b'\n') {
                        let line_bytes: Vec<u8> = raw_buf.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line_bytes).trim().to_string();

                        match process_sse_line(
                            &line,
                            &mut first_token_time,
                            &mut token_count,
                            &mut reasoning_chunks,
                            &mut last_finish_reason,
                            &tx,
                            &egui_ctx,
                        )
                        .await
                        {
                            LineOutcome::Continue => {}
                            LineOutcome::Done => {
                                finalize_stream(
                                    capture_ms,
                                    image_kb,
                                    user_start,
                                    first_token_time,
                                    token_count,
                                    reasoning_chunks,
                                    last_finish_reason.as_deref(),
                                    remaining_tokens,
                                    &tx,
                                    &egui_ctx,
                                )
                                .await;
                                return;
                            }
                            LineOutcome::Error(err) => {
                                let _ = tx.send(GroqEvent::Error(err)).await;
                                egui_ctx.request_repaint();
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(GroqEvent::Error(format!("Stream read error: {e}")))
                        .await;
                    egui_ctx.request_repaint();
                    return;
                }
            }
        }

        // Flush any remaining partial line at EOF when stream closes without trailing newline
        if !raw_buf.is_empty() {
            let tail = String::from_utf8_lossy(&raw_buf).trim().to_string();
            if let LineOutcome::Error(err) = process_sse_line(
                &tail,
                &mut first_token_time,
                &mut token_count,
                &mut reasoning_chunks,
                &mut last_finish_reason,
                &tx,
                &egui_ctx,
            )
            .await
            {
                let _ = tx.send(GroqEvent::Error(err)).await;
                egui_ctx.request_repaint();
                return;
            }
        }

        finalize_stream(
            capture_ms,
            image_kb,
            user_start,
            first_token_time,
            token_count,
            reasoning_chunks,
            last_finish_reason.as_deref(),
            remaining_tokens,
            &tx,
            &egui_ctx,
        )
        .await;
    }
}

fn find_last_image_url(messages: &[serde_json::Value]) -> Option<String> {
    messages.iter().rev().find_map(|m| {
        m.get("content").and_then(|c| c.as_array()).and_then(|arr| {
            arr.iter().find_map(|part| {
                if part.get("type").and_then(|t| t.as_str()) == Some("image_url") {
                    part.get("image_url")
                        .and_then(|u| u.get("url"))
                        .and_then(|url_val| url_val.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
    })
}

fn extract_message_text(msg: &serde_json::Value) -> String {
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    p.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        return parts.join("\n");
    }
    String::new()
}

/// Preserves caller system prompt, prior conversation turns, and caller user prompt alongside OCR text.
pub(crate) fn build_stage2_messages(
    messages: &[serde_json::Value],
    ocr_text: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let mut staged = Vec::new();
    let total_in = messages.len();

    for (idx, m) in messages.iter().enumerate() {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if idx + 1 == total_in && role == "user" {
            continue;
        }
        let text = extract_message_text(m);
        if !text.trim().is_empty() {
            staged.push(json!({ "role": role, "content": text }));
        }
    }

    let caller_user_text = messages
        .last()
        .map(extract_message_text)
        .unwrap_or_default();

    let combined_user_text = if caller_user_text.trim().is_empty() {
        format!("Screen content transcribed verbatim:\n{}", ocr_text.trim())
    } else {
        format!(
            "{}\n\nScreen content transcribed verbatim:\n{}",
            caller_user_text.trim(),
            ocr_text.trim()
        )
    };

    let staged_user_msg = json!({
        "role": "user",
        "content": combined_user_text
    });
    staged.push(staged_user_msg.clone());
    (staged, staged_user_msg)
}

async fn process_sse_line(
    line: &str,
    first_token_time: &mut Option<Instant>,
    token_count: &mut usize,
    reasoning_chunks: &mut usize,
    last_finish_reason: &mut Option<String>,
    tx: &mpsc::Sender<GroqEvent>,
    egui_ctx: &egui::Context,
) -> LineOutcome {
    if line.is_empty() || line.starts_with(':') {
        return LineOutcome::Continue;
    }

    let Some(raw_data) = line.strip_prefix("data:") else {
        return LineOutcome::Continue;
    };
    let data = raw_data.trim();
    if data == "[DONE]" {
        return LineOutcome::Done;
    }

    let Ok(parsed) = serde_json::from_str::<StreamChunk>(data) else {
        return LineOutcome::Continue;
    };

    if let Some(err) = parsed.error {
        return LineOutcome::Error(format!("Groq stream error: {err}"));
    }

    if let Some(choices) = parsed.choices {
        for choice in choices {
            if let Some(reason) = choice.finish_reason {
                *last_finish_reason = Some(reason);
            }
            if let Some(delta) = choice.delta {
                if let Some(r) = delta.reasoning {
                    if !r.is_empty() {
                        *reasoning_chunks += 1;
                        if *reasoning_chunks == 1 || (*reasoning_chunks).is_multiple_of(25) {
                            let _ = tx
                                .send(GroqEvent::Status(format!(
                                    "GPT-OSS-120B thinking... ({} chunks)",
                                    *reasoning_chunks
                                )))
                                .await;
                            egui_ctx.request_repaint();
                        }
                    }
                }
                if let Some(text) = delta.content {
                    if !text.is_empty() {
                        if first_token_time.is_none() {
                            *first_token_time = Some(Instant::now());
                        }
                        *token_count += 1;
                        let _ = tx.send(GroqEvent::Token(text)).await;
                        egui_ctx.request_repaint();
                    }
                }
            }
        }
    }

    LineOutcome::Continue
}

#[allow(clippy::too_many_arguments)]
async fn finalize_stream(
    capture_ms: u64,
    image_kb: usize,
    user_start: Instant,
    first_token_time: Option<Instant>,
    token_count: usize,
    reasoning_chunks: usize,
    finish_reason: Option<&str>,
    remaining_tokens: Option<u64>,
    tx: &mpsc::Sender<GroqEvent>,
    egui_ctx: &egui::Context,
) {
    if token_count == 0 {
        let msg = if finish_reason == Some("length") && reasoning_chunks > 0 {
            format!(
                "Model spent all tokens reasoning ({reasoning_chunks} chunks) before emitting code."
            )
        } else if reasoning_chunks > 0 {
            format!("Model ended after {reasoning_chunks} reasoning chunks with empty content.")
        } else {
            "Model returned an empty response.".to_string()
        };
        let _ = tx.send(GroqEvent::Error(msg)).await;
        egui_ctx.request_repaint();
        return;
    }

    let total_ms = user_start.elapsed().as_millis() as u64;
    let ttft_ms = first_token_time
        .map(|t| t.duration_since(user_start).as_millis() as u64)
        .unwrap_or(0);
    let stream_ms = first_token_time
        .map(|t| {
            user_start
                .elapsed()
                .saturating_sub(t.duration_since(user_start))
                .as_millis() as u64
        })
        .unwrap_or(1);
    let tokens_per_sec = if stream_ms > 0 {
        (token_count as f64) / (stream_ms as f64 / 1000.0)
    } else {
        0.0
    };
    let metrics = RequestMetrics {
        capture_ms,
        image_kb,
        ttft_ms,
        total_ms,
        token_count,
        tokens_per_sec,
        remaining_tokens,
        truncated: finish_reason == Some("length"),
    };

    let log_entry = format!(
        "[{:?}] WALL_CLOCK: {}ms | TTFT: {}ms | Screen: {}ms ({} KB) | {} tok ({:.1} tok/s) | Reasoning: {} | Finish: {:?} | RemTokens: {:?}\n",
        std::time::SystemTime::now(),
        total_ms,
        ttft_ms,
        capture_ms,
        image_kb,
        token_count,
        tokens_per_sec,
        reasoning_chunks,
        finish_reason,
        remaining_tokens
    );
    tokio::task::spawn_blocking(move || {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("cue_telemetry.log")
            .and_then(|mut f| std::io::Write::write_all(&mut f, log_entry.as_bytes()));
    });

    let _ = tx.send(GroqEvent::Done(Some(metrics))).await;
    egui_ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_pool_rotation() {
        let pool = KeyPool::new(vec![
            "gsk_key1".to_string(),
            "gsk_key2".to_string(),
            "gsk_key3".to_string(),
        ]);
        assert_eq!(pool.total_keys(), 3);
        assert_eq!(pool.get_current_key(), Some("gsk_key1".to_string()));

        let rot1 = pool.rotate_to_next();
        assert_eq!(rot1, Some((1, 2, "gsk_key2".to_string())));
        assert_eq!(pool.get_current_key(), Some("gsk_key2".to_string()));

        let rot2 = pool.rotate_to_next();
        assert_eq!(rot2, Some((2, 3, "gsk_key3".to_string())));
        assert_eq!(pool.get_current_key(), Some("gsk_key3".to_string()));

        let rot3 = pool.rotate_to_next();
        assert_eq!(rot3, Some((3, 1, "gsk_key1".to_string())));
        assert_eq!(pool.get_current_key(), Some("gsk_key1".to_string()));
    }

    #[test]
    fn test_key_pool_cooldown_skipping() {
        let pool = KeyPool::new(vec![
            "gsk_key1".to_string(),
            "gsk_key2".to_string(),
            "gsk_key3".to_string(),
        ]);
        pool.mark_cooldown(1, 60);

        let rot = pool.rotate_to_next();
        assert_eq!(rot, Some((1, 3, "gsk_key3".to_string())));
        assert_eq!(pool.get_current_key(), Some("gsk_key3".to_string()));
    }

    #[test]
    fn test_key_pool_all_cooldown_picks_soonest() {
        let pool = KeyPool::new(vec![
            "gsk_key1".to_string(),
            "gsk_key2".to_string(),
            "gsk_key3".to_string(),
        ]);
        let now = Instant::now();
        {
            let mut cd = pool.cooldowns.lock().unwrap();
            cd.insert(0, now + Duration::from_secs(60));
            cd.insert(1, now + Duration::from_secs(30));
            cd.insert(2, now + Duration::from_secs(5));
        }

        let rot = pool.rotate_to_next();
        assert_eq!(rot, Some((1, 3, "gsk_key3".to_string())));
    }

    #[test]
    fn test_single_key_no_rotation() {
        let pool = KeyPool::new(vec!["gsk_only_one".to_string()]);
        assert_eq!(pool.total_keys(), 1);
        assert_eq!(pool.rotate_to_next(), None);
    }

    #[test]
    fn test_cooldown_wait_uses_soonest_reset() {
        let pool = KeyPool::new(vec!["gsk_key1".to_string(), "gsk_key2".to_string()]);
        assert!(!pool.all_in_cooldown());
        assert_eq!(pool.soonest_cooldown_secs(), None);

        pool.mark_cooldown(0, 60);
        assert!(!pool.all_in_cooldown());

        pool.mark_cooldown(1, 2);
        assert!(pool.all_in_cooldown());
        assert!((1..=2).contains(&pool.soonest_cooldown_secs().unwrap()));
    }

    #[test]
    fn test_stage2_messages_preserves_caller_prompt_and_history() {
        let msgs = vec![
            json!({"role": "system", "content": "Caller system"}),
            json!({"role": "user", "content": "Turn 1 OCR problem"}),
            json!({"role": "assistant", "content": "```cpp\nint f(){}\n```"}),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "Fix the WA on test 3"},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,abc"}}
                ]
            }),
        ];

        let (staged, user_turn) = build_stage2_messages(&msgs, "Expected 6, got 5");
        assert_eq!(staged.len(), 4);
        assert_eq!(staged[0]["content"], "Caller system");
        assert_eq!(staged[1]["content"], "Turn 1 OCR problem");
        assert_eq!(staged[2]["content"], "```cpp\nint f(){}\n```");
        let last = staged[3]["content"].as_str().unwrap();
        assert!(last.contains("Fix the WA on test 3"));
        assert!(last.contains("Expected 6, got 5"));
        assert_eq!(user_turn["content"], staged[3]["content"]);
    }

    #[test]
    fn test_sse_ignores_empty_content_and_tracks_reasoning() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(16);
            let ctx = egui::Context::default();
            let mut ttft = None;
            let mut tok = 0;
            let mut reason = 0;
            let mut fin = None;

            // Opening empty content chunk must not start TTFT or token count
            let line1 = r#"data: {"choices":[{"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#;
            assert!(matches!(
                process_sse_line(line1, &mut ttft, &mut tok, &mut reason, &mut fin, &tx, &ctx).await,
                LineOutcome::Continue
            ));
            assert!(ttft.is_none());
            assert_eq!(tok, 0);

            // Reasoning chunk must increment reasoning counter, not visible token count
            let line2 = r#"data: {"choices":[{"delta":{"reasoning":"Analyze","channel":"analysis"},"finish_reason":null}]}"#;
            process_sse_line(line2, &mut ttft, &mut tok, &mut reason, &mut fin, &tx, &ctx).await;
            assert!(ttft.is_none());
            assert_eq!(tok, 0);
            assert_eq!(reason, 1);

            // Real content chunk sets TTFT and emits Token
            let line3 = r#"data:{"choices":[{"delta":{"content":"print(1)"},"finish_reason":"stop"}]}"#;
            process_sse_line(line3, &mut ttft, &mut tok, &mut reason, &mut fin, &tx, &ctx).await;
            assert!(ttft.is_some());
            assert_eq!(tok, 1);
            assert_eq!(fin.as_deref(), Some("stop"));

            // Verify Token event was emitted cleanly after Status event
            let mut got_token = None;
            while let Ok(ev) = rx.try_recv() {
                if let GroqEvent::Token(t) = ev {
                    got_token = Some(t);
                }
            }
            assert_eq!(got_token.as_deref(), Some("print(1)"));
        });
    }

    #[test]
    #[ignore = "requires GROQ_API_KEYS in .env and optional CUE_TEST_IMG"]
    fn test_live_two_stage_solve_and_fix() {
        use base64::prelude::*;
        let env_txt = std::fs::read_to_string(".env").expect("missing .env");
        let keys: Vec<String> = env_txt
            .lines()
            .find_map(|l| l.strip_prefix("GROQ_API_KEYS="))
            .expect("missing GROQ_API_KEYS")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("gsk_"))
            .collect();
        let pool = Arc::new(KeyPool::new(keys));
        let client = GroqClient::new_with_shared_pool(pool, reqwest::Client::new());

        let img_b64 = if let Ok(path) = std::env::var("CUE_TEST_IMG") {
            let bytes = std::fs::read(path).expect("read CUE_TEST_IMG");
            BASE64_STANDARD.encode(bytes)
        } else {
            crate::screen::capture_screen_jpeg_base64().expect("capture screen")
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel(128);
            let ctx = egui::Context::default();
            let start = Instant::now();
            tokio::spawn(async move {
                client
                    .stream_request(
                        Some("You are Cue, an elite coding interview and competitive programming assistant. NEVER explain concepts, teach, or lecture. Analyze problem constraints and edge cases carefully. Verify state transitions and select optimal algorithms to prevent TLE. Output ONLY the complete, optimal, runnable code solution inside a clean code block matching the editor language. Zero commentary, zero conversational filler, no code walkthrough.".to_string()),
                        "Solve the code problem shown on screen. Output ONLY the complete, working code solution in a code block. Do NOT explain how it works.".to_string(),
                        Some(img_b64),
                        12,
                        start,
                        ctx,
                        tx,
                    )
                    .await;
            });

            let mut out = String::new();
            let mut turn1_user = None;
            let mut metrics = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    GroqEvent::Token(t) => out.push_str(&t),
                    GroqEvent::Status(s) => println!("STATUS: {s}"),
                    GroqEvent::UserTurn(u) => turn1_user = Some(u),
                    GroqEvent::KeyRotated { from_index, to_index, reason } => {
                        println!("ROTATED: {from_index} -> {to_index} ({reason})");
                    }
                    GroqEvent::Done(m) => {
                        metrics = m;
                        break;
                    }
                    GroqEvent::Error(e) => panic!("Unexpected Groq error: {e}"),
                }
            }

            let m = metrics.expect("expected Done metrics");
            println!("METRICS: {:?}", m);
            println!("OUTPUT:\n{}", out);
            assert!(!out.trim().is_empty(), "result must not be empty");
            assert!(m.token_count > 0, "token_count must be > 0");
            let u1 = turn1_user.expect("expected UserTurn");
            println!("TURN1_USER:\n{}", u1["content"].as_str().unwrap());
            assert!(u1["content"].as_str().is_some(), "UserTurn must store plain OCR text");
        });
    }
}
