use eframe::egui;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct RequestMetrics {
    pub capture_ms: u64,
    pub image_kb: usize,
    pub ttft_ms: u64,
    pub total_ms: u64,
    pub token_count: usize,
    pub tokens_per_sec: f64,
    pub remaining_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum GroqEvent {
    Token(String),
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
        if self.keys.is_empty() {
            return None;
        }
        let idx = self.current_key_index();
        Some(self.keys[idx].clone())
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

    /// Rotates to the next available key that is not in cooldown.
    /// If all keys are in cooldown, picks the one that recovers soonest.
    pub fn rotate_to_next(&self) -> Option<(usize, usize, String)> {
        if self.keys.len() <= 1 {
            return None;
        }
        let total = self.keys.len();
        let old_idx = self.current_key_index();

        // 1. Try to find the next key that is NOT in cooldown
        for offset in 1..total {
            let candidate_idx = (old_idx + offset) % total;
            if !self.is_key_in_cooldown(candidate_idx) {
                self.current_index.store(candidate_idx, Ordering::SeqCst);
                return Some((old_idx + 1, candidate_idx + 1, self.keys[candidate_idx].clone()));
            }
        }

        // 2. All keys are in cooldown: select next sequentially
        let next_idx = (old_idx + 1) % total;
        self.current_index.store(next_idx, Ordering::SeqCst);
        Some((old_idx + 1, next_idx + 1, self.keys[next_idx].clone()))
    }
}

pub struct GroqClient {
    pool: KeyPool,
    http: reqwest::Client,
    pub fast_model: String,
    pub vision_model: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct StreamChoice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
}

impl GroqClient {
    pub fn new(pool: KeyPool) -> Self {
        Self::new_with_client(pool, reqwest::Client::new())
    }

    pub fn new_with_client(pool: KeyPool, http: reqwest::Client) -> Self {
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
        let image_kb = image_base64.as_ref().map(|s| (s.len() * 3) / 4 / 1024).unwrap_or(0);
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

        self.stream_messages_request(messages, capture_ms, image_kb, user_start, egui_ctx, tx).await;
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

        let max_retries = self.pool.total_keys();
        let mut attempts = 0;

        let has_image = messages.iter().any(|m| {
            if let Some(content) = m.get("content") {
                if let Some(arr) = content.as_array() {
                    arr.iter().any(|part| part.get("type").and_then(|t| t.as_str()) == Some("image_url"))
                } else {
                    false
                }
            } else {
                false
            }
        });

        let mut final_messages = messages.clone();
        let mut final_model = if has_image {
            self.vision_model.clone()
        } else {
            self.fast_model.clone()
        };

        // Two-Stage Vision Pipeline:
        // Stage 1: Qwen-27B extracts precise problem description, code, and test failure from screen
        // Stage 2: 120B reasoning powerhouse (openai/gpt-oss-120b) writes flawless, optimal code
        if has_image {
            let img_url_opt = messages.iter().find_map(|m| {
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
            });

            if let Some(img_url) = img_url_opt {
                if let Some(current_key) = self.pool.get_current_key() {
                    let ocr_payload = json!({
                        "model": &self.vision_model,
                        "messages": [
                            {
                                "role": "user",
                                "content": [
                                    {
                                        "type": "text",
                                        "text": "Transcribe the following from this coding screen in full detail:\n\
                                                 1. The Problem Title, Full Description, Rules, and Constraints on screen.\n\
                                                 2. The current code in the code editor.\n\
                                                 3. The failing test case (Input, Output, Expected) or runtime error in the test result panel."
                                    },
                                    {
                                        "type": "image_url",
                                        "image_url": { "url": img_url }
                                    }
                                ]
                            }
                        ],
                        "temperature": 0.1,
                        "max_completion_tokens": 512
                    });

                    if let Ok(res) = self
                        .http
                        .post("https://api.groq.com/openai/v1/chat/completions")
                        .header("Authorization", format!("Bearer {}", current_key))
                        .header("Content-Type", "application/json")
                        .json(&ocr_payload)
                        .send()
                        .await
                    {
                        if res.status().is_success() {
                            if let Ok(data) = res.json::<serde_json::Value>().await {
                                if let Some(ocr_text) = data["choices"][0]["message"]["content"].as_str() {
                                    if !ocr_text.trim().is_empty() {
                                        final_model = self.fast_model.clone();
                                        final_messages = vec![
                                            json!({
                                                "role": "system",
                                                "content": "You are Cue, an elite coding interview and competitive programming assistant. \
                                                            Analyze constraints and state transitions carefully. Pick optimal algorithms to prevent TLE. \
                                                            Output ONLY the complete, optimal, runnable code solution inside a clean code block. \
                                                            Zero commentary, zero conversational filler, no code walkthrough."
                                            }),
                                            json!({
                                                "role": "user",
                                                "content": format!(
                                                    "Problem context transcribed from screen:\n{}\n\n\
                                                     Task: Write the complete, optimal runnable code solution to pass all test cases. Output ONLY the code block.",
                                                    ocr_text
                                                )
                                            })
                                        ];
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let model = final_model;
        let messages = final_messages;

        while attempts < max_retries {
            let key_idx = self.pool.current_key_index();
            let current_key = match self.pool.get_current_key() {
                Some(k) => k,
                None => break,
            };

            // Build request conforming to Groq API Reference
            let payload = json!({
                "model": model,
                "messages": messages,
                "stream": true,
                "temperature": 0.2,
                "max_completion_tokens": 1024
            });

            let res = self
                .http
                .post("https://api.groq.com/openai/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", current_key))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await;

            match res {
                Ok(response) => {
                    let status = response.status();
                    let headers = response.headers().clone();

                    // Parse rate limit headers
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
                        // Rate limit (RPM, RPD, TPM, or TPD) hit on this key
                        attempts += 1;
                        self.pool.mark_cooldown(key_idx, retry_after_secs);

                        if let Some((from_i, to_i, _next_key)) = self.pool.rotate_to_next() {
                            let _ = tx
                                .send(GroqEvent::KeyRotated {
                                    from_index: from_i,
                                    to_index: to_i,
                                    reason: format!("429 Rate Limit (Retry after {retry_after_secs}s)"),
                                })
                                .await;
                            egui_ctx.request_repaint();
                            continue; // Retry with next key immediately!
                        } else {
                            let _ = tx
                                .send(GroqEvent::Error(format!(
                                    "Rate limit reached (429). Retry in {retry_after_secs}s."
                                )))
                                .await;
                            egui_ctx.request_repaint();
                            return;
                        }
                    }

                    if !status.is_success() {
                        let err_text = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Unknown error".to_string());
                        let _ = tx
                            .send(GroqEvent::Error(format!(
                                "Groq API error ({status}): {err_text}"
                            )))
                            .await;
                        egui_ctx.request_repaint();
                        return;
                    }

                    // Preemptive rotation check: if key is nearly exhausted (< 2100 tokens left),
                    // mark cooldown so next request proactively uses a fresh key from the pool!
                    if let Some(rem_tok) = remaining_tokens {
                        if rem_tok < 2100 {
                            self.pool.mark_cooldown(key_idx, 60);
                            let _ = self.pool.rotate_to_next();
                        }
                    }

                    // Success 200 OK -> stream response SSE
                    let mut stream = response.bytes_stream();
                    let mut buffer = String::new();
                    let mut first_token_time: Option<Instant> = None;
                    let mut token_count = 0usize;

                    while let Some(chunk_res) = stream.next().await {
                        match chunk_res {
                            Ok(chunk) => {
                                let chunk_str = String::from_utf8_lossy(&chunk);
                                buffer.push_str(&chunk_str);

                                while let Some(pos) = buffer.find('\n') {
                                    let line = buffer[..pos].trim().to_string();
                                    buffer = buffer[pos + 1..].to_string();

                                    if line.is_empty() || line.starts_with(':') {
                                        continue;
                                    }

                                    if let Some(data) = line.strip_prefix("data: ") {
                                        if data.trim() == "[DONE]" {
                                            let total_ms = user_start.elapsed().as_millis() as u64;
                                            let ttft_ms = first_token_time
                                                .map(|t| t.duration_since(user_start).as_millis() as u64)
                                                .unwrap_or(0);
                                            let stream_ms = first_token_time
                                                .map(|t| user_start.elapsed().saturating_sub(t.duration_since(user_start)).as_millis() as u64)
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
                                            };
                                            let log_entry = format!(
                                                "[{:?}] WALL_CLOCK: {}ms | TTFT: {}ms | Screen: {}ms ({} KB) | {} tokens ({:.1} tok/s) | RemTokens: {:?}\n",
                                                std::time::SystemTime::now(),
                                                total_ms,
                                                ttft_ms,
                                                capture_ms,
                                                image_kb,
                                                token_count,
                                                tokens_per_sec,
                                                remaining_tokens
                                            );
                                            let _ = std::fs::OpenOptions::new()
                                                .create(true)
                                                .append(true)
                                                .open("cue_telemetry.log")
                                                .and_then(|mut f| std::io::Write::write_all(&mut f, log_entry.as_bytes()));

                                            let _ = tx.send(GroqEvent::Done(Some(metrics))).await;
                                            egui_ctx.request_repaint();
                                            return;
                                        }

                                        if let Ok(parsed) = serde_json::from_str::<StreamChunk>(data) {
                                            if let Some(choices) = parsed.choices {
                                                for choice in choices {
                                                    if let Some(delta) = choice.delta {
                                                        if let Some(text) = delta.content {
                                                            if first_token_time.is_none() {
                                                                first_token_time = Some(Instant::now());
                                                            }
                                                            token_count += 1;
                                                            let _ = tx
                                                                .send(GroqEvent::Token(text))
                                                                .await;
                                                            egui_ctx.request_repaint();
                                                        }
                                                    }
                                                }
                                            }
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

                    let total_ms = user_start.elapsed().as_millis() as u64;
                    let ttft_ms = first_token_time
                        .map(|t| t.duration_since(user_start).as_millis() as u64)
                        .unwrap_or(0);
                    let stream_ms = first_token_time
                        .map(|t| user_start.elapsed().saturating_sub(t.duration_since(user_start)).as_millis() as u64)
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
                    };
                    let log_entry = format!(
                        "[{:?}] WALL_CLOCK: {}ms | TTFT: {}ms | Screen: {}ms ({} KB) | {} tokens ({:.1} tok/s) | RemTokens: {:?}\n",
                        std::time::SystemTime::now(),
                        total_ms,
                        ttft_ms,
                        capture_ms,
                        image_kb,
                        token_count,
                        tokens_per_sec,
                        remaining_tokens
                    );
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("cue_telemetry.log")
                        .and_then(|mut f| std::io::Write::write_all(&mut f, log_entry.as_bytes()));

                    let _ = tx.send(GroqEvent::Done(Some(metrics))).await;
                    egui_ctx.request_repaint();
                    return;
                }
                Err(e) => {
                    let _ = tx
                        .send(GroqEvent::Error(format!("Network connection failed: {e}")))
                        .await;
                    egui_ctx.request_repaint();
                    return;
                }
            }
        }

        let _ = tx
            .send(GroqEvent::Error(format!(
                "All {} configured Groq API keys are exhausted or rate-limited.",
                self.pool.total_keys()
            )))
            .await;
    }
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
        // Put key 2 in cooldown
        pool.mark_cooldown(1, 60);

        // Rotating from key 1 should skip key 2 and jump straight to key 3!
        let rot = pool.rotate_to_next();
        assert_eq!(rot, Some((1, 3, "gsk_key3".to_string())));
        assert_eq!(pool.get_current_key(), Some("gsk_key3".to_string()));
    }

    #[test]
    fn test_single_key_no_rotation() {
        let pool = KeyPool::new(vec!["gsk_only_one".to_string()]);
        assert_eq!(pool.total_keys(), 1);
        assert_eq!(pool.rotate_to_next(), None);
    }
}
