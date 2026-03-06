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
