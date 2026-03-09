use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use hound::{SampleFormat, WavReader};
use reqwest::StatusCode;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::TtsSection;

const CACHE_CAPACITY: usize = 64;
const UVX_COMMAND: &str = "uvx";

struct CacheState {
    values: HashMap<String, Vec<i16>>,
    order: VecDeque<String>,
}

impl CacheState {
    fn new() -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &str) -> Option<Vec<i16>> {
        let value = self.values.get(key).cloned()?;
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
        Some(value)
    }

    fn insert(&mut self, key: String, value: Vec<i16>) {
        if self.values.contains_key(&key) {
            self.values.insert(key.clone(), value);
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
            self.order.push_back(key);
            return;
        }

        if self.values.len() >= CACHE_CAPACITY
            && let Some(oldest) = self.order.pop_front()
        {
            self.values.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.values.insert(key, value);
    }
}

pub struct PocketTtsRuntime {
    config: TtsSection,
    sample_rate: u32,
    http: reqwest::Client,
    sidecar: Mutex<Option<Child>>,
    cache: Mutex<CacheState>,
}

impl PocketTtsRuntime {
    pub fn new(config: TtsSection, sample_rate: u32) -> anyhow::Result<Self> {
        let timeout = Duration::from_millis(config.request_timeout_ms);
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build pocket-tts HTTP client")?;

        Ok(Self {
            config,
            sample_rate,
            http,
            sidecar: Mutex::new(None),
            cache: Mutex::new(CacheState::new()),
        })
    }

    pub async fn startup(&self) -> anyhow::Result<()> {
        self.ensure_server_ready(true).await
    }

    pub async fn synth_channel_announcement(
        &self,
        channel_name: Option<&str>,
        channel_id: u32,
    ) -> anyhow::Result<Vec<i16>> {
        let phrase = channel_announcement_phrase(channel_name, channel_id);
        self.synthesize_phrase(&phrase).await
    }

    pub async fn synthesize_phrase(&self, phrase: &str) -> anyhow::Result<Vec<i16>> {
        let cache_key = format!("{}|{}|{}", self.config.voice, self.sample_rate, phrase);
        if let Some(pcm) = self.cache.lock().await.get(&cache_key) {
            return Ok(pcm);
        }

        self.ensure_server_ready(self.config.auto_restart).await?;

        let mut form = vec![("text", phrase.to_string())];
        if !self.config.voice.trim().is_empty() {
            form.push(("voice_url", self.config.voice.clone()));
        }

        let resp = self
            .http
            .post(format!(
                "http://{}:{}/tts",
                self.config.host, self.config.port
            ))
            .form(&form)
            .send()
            .await
            .context("failed to call pocket-tts /tts endpoint")?;

        if resp.status() != StatusCode::OK {
            return Err(anyhow::anyhow!(
                "pocket-tts returned non-OK status {}",
                resp.status()
            ));
        }

        let bytes = resp
            .bytes()
            .await
            .context("failed to read pocket-tts audio")?;
        let pcm = decode_and_resample_wav(&bytes, self.sample_rate)?;
        self.cache.lock().await.insert(cache_key, pcm.clone());
        Ok(pcm)
    }

    async fn ensure_server_ready(&self, allow_spawn: bool) -> anyhow::Result<()> {
        if self.health_check().await {
            return Ok(());
        }

        if !allow_spawn {
            return Err(anyhow::anyhow!(
                "pocket-tts sidecar not ready and auto_restart=false"
            ));
        }

        self.spawn_sidecar_if_needed().await?;

        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(self.config.startup_timeout_ms);
        loop {
            if self.health_check().await {
                return Ok(());
            }
            self.fail_if_sidecar_exited().await?;
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "pocket-tts sidecar did not become healthy within {}ms",
                    self.config.startup_timeout_ms
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn health_check(&self) -> bool {
        match self
            .http
            .get(format!(
                "http://{}:{}/health",
                self.config.host, self.config.port
            ))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    async fn spawn_sidecar_if_needed(&self) -> anyhow::Result<()> {
        let mut sidecar = self.sidecar.lock().await;
        if let Some(child) = sidecar.as_mut() {
            match child.try_wait() {
                Ok(None) => return Ok(()),
                Ok(Some(status)) => {
                    warn!("pocket-tts sidecar exited with status {}", status);
                    *sidecar = None;
                }
                Err(err) => {
                    warn!("failed checking pocket-tts sidecar status: {}", err);
                    *sidecar = None;
                }
            }
        }

        debug!(
            "starting pocket-tts sidecar: {} pocket-tts serve --port {}",
            UVX_COMMAND, self.config.port
        );
        let child = Command::new(UVX_COMMAND)
            .arg("pocket-tts")
            .arg("serve")
            .arg("--port")
            .arg(self.config.port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn pocket-tts sidecar via uvx")?;

        *sidecar = Some(child);
        Ok(())
    }

    async fn fail_if_sidecar_exited(&self) -> anyhow::Result<()> {
        let mut sidecar = self.sidecar.lock().await;
        let Some(child) = sidecar.as_mut() else {
            return Ok(());
        };

        if let Some(status) = child
            .try_wait()
            .context("failed to query pocket-tts sidecar status")?
        {
            *sidecar = None;
            return Err(anyhow::anyhow!(
                "pocket-tts sidecar exited before becoming ready (status={})",
                status
            ));
        }
        Ok(())
    }
}

pub fn channel_announcement_phrase(channel_name: Option<&str>, channel_id: u32) -> String {
    let name = channel_name.unwrap_or("").trim();
    if name.is_empty() {
        return format!("You are now in channel {}", channel_id);
    }
    format!("You are now in the {} channel", name)
}

pub fn text_message_phrase(sender_name: Option<&str>, raw_message: &str) -> String {
    let sender = sender_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("Someone");
    let analysis = analyze_text_message_for_tts(raw_message);
    if analysis.non_link_token_count == 0 && !analysis.link_hosts_for_speech.is_empty() {
        if analysis.link_hosts_for_speech.len() == 1 {
            return format!(
                "{sender} posted a link to {}",
                analysis.link_hosts_for_speech[0]
            );
        }
        return format!(
            "{sender} posted links to {}",
            join_human_list(&analysis.link_hosts_for_speech)
        );
    }
    if analysis.normalized_message.is_empty() {
        return format!("{sender} sent a message");
    }
    format!("{sender} says: {}", analysis.normalized_message)
}

struct TextMessageTtsAnalysis {
    normalized_message: String,
    link_hosts_for_speech: Vec<String>,
    non_link_token_count: usize,
}

fn analyze_text_message_for_tts(raw_message: &str) -> TextMessageTtsAnalysis {
    let with_anchor_hrefs = replace_html_anchor_tags(raw_message);
    let without_html_tags = strip_html_tags(&with_anchor_hrefs);
    let decoded_entities = decode_basic_html_entities(&without_html_tags);

    let mut normalized_tokens = Vec::new();
    let mut link_hosts_for_speech = Vec::new();
    let mut non_link_token_count = 0usize;

    for token in decoded_entities.split_whitespace() {
        let (normalized_token, host_for_speech) = normalize_text_token_for_tts(token);
        if let Some(host) = host_for_speech {
            link_hosts_for_speech.push(host);
        } else if token_has_spoken_content(&normalized_token) {
            non_link_token_count += 1;
        }
        normalized_tokens.push(normalized_token);
    }

    TextMessageTtsAnalysis {
        normalized_message: collapse_whitespace(&normalized_tokens.join(" ")),
        link_hosts_for_speech,
        non_link_token_count,
    }
}

fn replace_html_anchor_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;

    while cursor < input.len() {
        let Some(anchor_start_rel) = find_ascii_case_insensitive(&input[cursor..], "<a") else {
            out.push_str(&input[cursor..]);
            break;
        };
        let anchor_start = cursor + anchor_start_rel;
        out.push_str(&input[cursor..anchor_start]);

        let Some(open_tag_end_rel) = input[anchor_start..].find('>') else {
            out.push_str(&input[anchor_start..]);
            break;
        };
        let open_tag_end = anchor_start + open_tag_end_rel;
        let open_tag = &input[anchor_start..=open_tag_end];
        let inner_start = open_tag_end + 1;

        let Some(close_tag_rel) = find_ascii_case_insensitive(&input[inner_start..], "</a>") else {
            out.push_str(&input[anchor_start..]);
            break;
        };
        let inner_end = inner_start + close_tag_rel;
        let close_tag_end = inner_end + "</a>".len();
        let inner_text = &input[inner_start..inner_end];

        let replacement = parse_anchor_href(open_tag).unwrap_or_else(|| inner_text.to_string());
        out.push(' ');
        out.push_str(&replacement);
        out.push(' ');

        cursor = close_tag_end;
    }

    out
}

fn parse_anchor_href(open_tag: &str) -> Option<String> {
    let open_tag_lower = open_tag.to_ascii_lowercase();
    let href_idx = open_tag_lower.find("href")?;
    let mut value = &open_tag[href_idx + "href".len()..];
    value = value.trim_start();
    if !value.starts_with('=') {
        return None;
    }
    value = value[1..].trim_start();
    if value.is_empty() {
        return None;
    }

    let first_char = value.as_bytes()[0] as char;
    if first_char == '"' || first_char == '\'' {
        let quote = first_char;
        let rest = &value[1..];
        let end = rest.find(quote)?;
        let href = &rest[..end];
        return Some(decode_basic_html_entities(href));
    }

    let end = value
        .char_indices()
        .find_map(|(idx, ch)| (ch.is_whitespace() || ch == '>').then_some(idx))
        .unwrap_or(value.len());
    let href = &value[..end];
    Some(decode_basic_html_entities(href))
}

fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn decode_basic_html_entities(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn token_has_spoken_content(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_alphanumeric())
}

fn join_human_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        _ => {
            let mut out = String::new();
            for (idx, item) in items.iter().enumerate() {
                if idx > 0 {
                    if idx == items.len() - 1 {
                        out.push_str(", and ");
                    } else {
                        out.push_str(", ");
                    }
                }
                out.push_str(item);
            }
            out
        }
    }
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let haystack_bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.len() > haystack_bytes.len() {
        return None;
    }
    haystack_bytes
        .windows(needle_bytes.len())
        .position(|window| window.eq_ignore_ascii_case(needle_bytes))
}

fn normalize_text_token_for_tts(token: &str) -> (String, Option<String>) {
    let (prefix, core, suffix) = split_token_affixes(token);
    if core.is_empty() {
        return (token.to_string(), None);
    }
    if let Some(hostname) = extract_url_host(core) {
        let spoken_host = hostname_for_speech(&hostname);
        return (format!("{prefix}{spoken_host}{suffix}"), Some(spoken_host));
    }
    (token.to_string(), None)
}

fn split_token_affixes(token: &str) -> (&str, &str, &str) {
    let start = token
        .char_indices()
        .find(|(_, c)| !is_leading_punctuation(*c))
        .map(|(idx, _)| idx)
        .unwrap_or(token.len());
    let end = token
        .char_indices()
        .rev()
        .find(|(_, c)| !is_trailing_punctuation(*c))
        .map(|(idx, c)| idx + c.len_utf8())
        .unwrap_or(start);
    if end < start {
        return (token, "", "");
    }
    (&token[..start], &token[start..end], &token[end..])
}

fn is_leading_punctuation(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '`')
}

fn is_trailing_punctuation(c: char) -> bool {
    matches!(
        c,
        ')' | ']' | '}' | '>' | '.' | ',' | '!' | '?' | ';' | ':' | '"' | '\'' | '`'
    )
}

fn extract_url_host(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return parse_host_from_url(token);
    }
    if lower.starts_with("www.") {
        let candidate = format!("https://{token}");
        return parse_host_from_url(&candidate);
    }
    parse_host_from_bare_token(token)
}

fn parse_host_from_url(url: &str) -> Option<String> {
    let without_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("HTTP://"))
        .or_else(|| url.strip_prefix("HTTPS://"))?;
    let host_and_port = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = host_and_port.split(':').next().unwrap_or_default();
    normalize_host_for_speech(host)
}

fn parse_host_from_bare_token(token: &str) -> Option<String> {
    if token.contains("://") {
        return None;
    }
    let host_and_port = token
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let host = host_and_port.split(':').next().unwrap_or_default();
    normalize_host_for_speech(host)
}

fn normalize_host_for_speech(host: &str) -> Option<String> {
    let host = host.trim().trim_matches('.');
    if host.is_empty() {
        return None;
    }
    let host = host
        .strip_prefix("www.")
        .unwrap_or(host)
        .to_ascii_lowercase();
    if !is_valid_hostname(&host) {
        return None;
    }
    Some(host)
}

fn is_valid_hostname(host: &str) -> bool {
    if !host.contains('.') {
        return false;
    }
    let mut labels = host.split('.');
    let last = match labels.next_back() {
        Some(value) => value,
        None => return false,
    };
    if !last.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    host.split('.').all(is_valid_host_label)
}

fn is_valid_host_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn hostname_for_speech(host: &str) -> String {
    let spoken = host
        .split('.')
        .map(|label| label.replace('-', " dash "))
        .collect::<Vec<_>>()
        .join(" dot ");
    collapse_whitespace(&spoken)
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_and_resample_wav(wav_bytes: &[u8], output_sample_rate: u32) -> anyhow::Result<Vec<i16>> {
    let normalized = normalize_streamed_wav(wav_bytes);
    let mut reader =
        WavReader::new(Cursor::new(normalized.as_slice())).context("invalid WAV response")?;
    let spec = reader.spec();
    if spec.channels == 0 {
        return Err(anyhow::anyhow!("WAV response has zero channels"));
    }

    let samples_f32 = match (spec.sample_format, spec.bits_per_sample) {
        // Use i32 for all integer WAV input because valid bits can be packed
        // into a wider container (e.g. 16-bit in 32-bit container).
        (SampleFormat::Int, 1..=32) => read_int_samples_to_f32(&mut reader, spec.bits_per_sample)?,
        (SampleFormat::Float, 32) => read_float_samples_to_f32::<f32>(&mut reader)?,
        _ => {
            return Err(anyhow::anyhow!(
                "unsupported WAV format: {:?} {} bits",
                spec.sample_format,
                spec.bits_per_sample
            ));
        }
    };

    let mono = mix_to_mono(&samples_f32, spec.channels as usize);
    let resampled = if spec.sample_rate == output_sample_rate {
        mono
    } else {
        linear_resample(&mono, spec.sample_rate, output_sample_rate)
    };

    Ok(resampled
        .into_iter()
        .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect())
}

fn normalize_streamed_wav(wav_bytes: &[u8]) -> Vec<u8> {
    // Pocket-TTS streams WAV over chunked HTTP and uses a placeholder
    // frame count for unseekable output. That can produce oversized RIFF/data
    // length fields which strict decoders interpret as truncated files.
    if wav_bytes.len() < 12 || &wav_bytes[0..4] != b"RIFF" || &wav_bytes[8..12] != b"WAVE" {
        return wav_bytes.to_vec();
    }

    let mut out = wav_bytes.to_vec();
    let riff_size = (out.len().saturating_sub(8)).min(u32::MAX as usize) as u32;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());

    let mut offset = 12usize;
    while offset + 8 <= out.len() {
        let chunk_id = [
            out[offset],
            out[offset + 1],
            out[offset + 2],
            out[offset + 3],
        ];
        let declared_size = u32::from_le_bytes([
            out[offset + 4],
            out[offset + 5],
            out[offset + 6],
            out[offset + 7],
        ]) as usize;
        let data_start = offset + 8;
        if data_start > out.len() {
            break;
        }

        if &chunk_id == b"data" {
            let actual_size = (out.len() - data_start).min(u32::MAX as usize) as u32;
            if declared_size != actual_size as usize {
                out[offset + 4..offset + 8].copy_from_slice(&actual_size.to_le_bytes());
            }
            break;
        }

        let chunk_total = 8usize
            .saturating_add(declared_size)
            .saturating_add(declared_size % 2);
        if chunk_total == 0 || offset.saturating_add(chunk_total) > out.len() {
            break;
        }
        offset += chunk_total;
    }

    out
}

fn read_int_samples_to_f32(
    reader: &mut WavReader<Cursor<&[u8]>>,
    bits_per_sample: u16,
) -> anyhow::Result<Vec<f32>> {
    let effective_bits = bits_per_sample.clamp(1, 32) as u32;
    let max = (1_i64 << effective_bits.saturating_sub(1)).max(1) as f32;
    let mut out = Vec::new();
    for sample in reader.samples::<i32>() {
        let v =
            sample.map_err(|err| anyhow::anyhow!("failed reading integer WAV sample: {}", err))?;
        out.push((v as f32) / max);
    }
    Ok(out)
}

fn read_float_samples_to_f32<T>(reader: &mut WavReader<Cursor<&[u8]>>) -> anyhow::Result<Vec<f32>>
where
    T: hound::Sample + Into<f64>,
{
    let mut out = Vec::new();
    for sample in reader.samples::<T>() {
        let v = sample.context("failed reading float WAV sample")?;
        out.push(v.into() as f32);
    }
    Ok(out)
}

fn mix_to_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }

    let mut mono = Vec::with_capacity(samples.len() / channels + 1);
    for frame in samples.chunks(channels) {
        let sum: f32 = frame.iter().copied().sum();
        mono.push(sum / frame.len() as f32);
    }
    mono
}

fn linear_resample(input: &[f32], input_rate: u32, output_rate: u32) -> Vec<f32> {
    if input.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if input_rate == output_rate {
        return input.to_vec();
    }

    let ratio = output_rate as f64 / input_rate as f64;
    let output_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(output_len.max(1));

    for i in 0..output_len {
        let src_pos = i as f64 / ratio;
        let left = src_pos.floor() as usize;
        let right = (left + 1).min(input.len().saturating_sub(1));
        let frac = (src_pos - left as f64) as f32;
        let sample = input[left] + (input[right] - input[left]) * frac;
        out.push(sample);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        CacheState, channel_announcement_phrase, decode_and_resample_wav, linear_resample,
        text_message_phrase,
    };
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::io::Cursor;

    #[test]
    fn announcement_phrase_uses_name_when_available() {
        let phrase = channel_announcement_phrase(Some("SIP Calls"), 7);
        assert_eq!(phrase, "You are now in the SIP Calls channel");
    }

    #[test]
    fn announcement_phrase_falls_back_to_id() {
        let phrase = channel_announcement_phrase(Some("   "), 42);
        assert_eq!(phrase, "You are now in channel 42");
    }

    #[test]
    fn text_message_phrase_includes_sender_name() {
        let phrase = text_message_phrase(Some("Sam"), "hello there");
        assert_eq!(phrase, "Sam says: hello there");
    }

    #[test]
    fn text_message_phrase_uses_default_sender_when_unknown() {
        let phrase = text_message_phrase(None, "hello there");
        assert_eq!(phrase, "Someone says: hello there");
    }

    #[test]
    fn text_message_phrase_normalizes_https_url() {
        let phrase = text_message_phrase(Some("Sam"), "check https://alamode.dev");
        assert_eq!(phrase, "Sam says: check alamode dot dev");
    }

    #[test]
    fn text_message_phrase_normalizes_www_url() {
        let phrase = text_message_phrase(Some("Sam"), "check www.alamode.dev now");
        assert_eq!(phrase, "Sam says: check alamode dot dev now");
    }

    #[test]
    fn text_message_phrase_normalizes_bare_host_with_path_and_query() {
        let phrase = text_message_phrase(Some("Sam"), "go to alamode.dev/black/brie?legal=yes");
        assert_eq!(phrase, "Sam says: go to alamode dot dev");
    }

    #[test]
    fn text_message_phrase_preserves_mixed_sentence_text() {
        let phrase = text_message_phrase(
            Some("Sam"),
            "Please review this: https://alamode.dev/black/brie?even=legal thanks!",
        );
        assert_eq!(
            phrase,
            "Sam says: Please review this: alamode dot dev thanks!"
        );
    }

    #[test]
    fn text_message_phrase_normalizes_html_anchor_href_url() {
        let phrase = text_message_phrase(
            Some("Sam"),
            r#"<a href="https://www.google.com/maps/place/Denver+Turnverein?entry=ttu&amp;g_ep=abc">https://www.google.com/maps/place/Denver+Turnverein?entry=ttu&amp;g_ep=abc</a>"#,
        );
        assert_eq!(phrase, "Sam posted a link to google dot com");
    }

    #[test]
    fn text_message_phrase_uses_posted_link_wording_for_plain_link_message() {
        let phrase = text_message_phrase(Some("Sam"), "https://www.google.com/maps/place/Denver");
        assert_eq!(phrase, "Sam posted a link to google dot com");
    }

    #[test]
    fn text_message_phrase_preserves_text_around_html_anchor() {
        let phrase = text_message_phrase(
            Some("Sam"),
            r#"Meet at <a href="https://www.google.com/maps/place/Denver">map</a> tonight"#,
        );
        assert_eq!(phrase, "Sam says: Meet at google dot com tonight");
    }

    #[test]
    fn linear_resample_scales_length() {
        let src = vec![0.0, 0.5, 1.0, 0.5];
        let out = linear_resample(&src, 24_000, 48_000);
        assert_eq!(out.len(), src.len() * 2);
    }

    #[test]
    fn cache_state_updates_lru_order() {
        let mut cache = CacheState::new();
        cache.insert("a".to_string(), vec![1]);
        cache.insert("b".to_string(), vec![2]);
        let _ = cache.get("a");
        assert_eq!(cache.order.back().map(String::as_str), Some("a"));
    }

    #[test]
    fn decodes_streamed_wav_with_oversized_lengths() {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = WavWriter::new(&mut buf, spec).expect("writer");
            for _ in 0..480 {
                writer.write_sample(1000i16).expect("sample");
            }
            writer.finalize().expect("finalize");
        }

        let mut wav = buf.into_inner();
        // Simulate unseekable-stream header mismatch:
        // huge RIFF/data sizes, but short payload.
        wav[4..8].copy_from_slice(&0x7fff_ff00u32.to_le_bytes());
        let mut offset = 12usize;
        while offset + 8 <= wav.len() {
            if &wav[offset..offset + 4] == b"data" {
                wav[offset + 4..offset + 8].copy_from_slice(&0x7fff_ff00u32.to_le_bytes());
                break;
            }
            let size = u32::from_le_bytes([
                wav[offset + 4],
                wav[offset + 5],
                wav[offset + 6],
                wav[offset + 7],
            ]) as usize;
            let step = 8 + size + (size % 2);
            offset += step;
        }

        let pcm = decode_and_resample_wav(&wav, 48_000).expect("decode");
        assert!(!pcm.is_empty());
    }
}
