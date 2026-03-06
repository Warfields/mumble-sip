use serde::Deserialize;

use crate::mumble::control::MumbleConfig;
use crate::sip::SipConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub sip: SipSection,
    pub mumble: MumbleSection,
    #[serde(default)]
    pub audio: AudioSection,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SipSection {
    #[serde(default = "default_sip_port")]
    pub listen_port: u16,
    pub account_uri: String,
    #[serde(default)]
    pub registrar: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "default_max_calls")]
    pub max_concurrent_calls: u32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MumbleSection {
    pub default_host: String,
    #[serde(default = "default_mumble_port")]
    pub port: u16,
    #[serde(default = "default_mumble_username")]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default = "default_true")]
    pub accept_invalid_cert: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AudioSection {
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_frame_duration")]
    pub frame_duration_ms: u32,
    #[serde(default = "default_opus_bitrate")]
    pub opus_bitrate: u32,
    #[serde(default = "default_jitter_buffer")]
    pub jitter_buffer_ms: u32,
}

impl Default for AudioSection {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            frame_duration_ms: default_frame_duration(),
            opus_bitrate: default_opus_bitrate(),
            jitter_buffer_ms: default_jitter_buffer(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn sip_config(&self) -> SipConfig {
        SipConfig {
            listen_port: self.sip.listen_port,
            account_uri: self.sip.account_uri.clone(),
            registrar: self.sip.registrar.clone(),
            username: self.sip.username.clone(),
            password: self.sip.password.clone(),
            max_concurrent_calls: self.sip.max_concurrent_calls,
        }
    }

    pub fn mumble_config(&self, host_override: Option<&str>) -> MumbleConfig {
        MumbleConfig {
            host: host_override
                .unwrap_or(&self.mumble.default_host)
                .to_string(),
            port: self.mumble.port,
            username: self.mumble.username.clone(),
            password: self.mumble.password.clone(),
            channel: self.mumble.channel.clone(),
            accept_invalid_cert: self.mumble.accept_invalid_cert,
        }
    }
}

fn default_sip_port() -> u16 { 5060 }
fn default_max_calls() -> u32 { 10 }
fn default_mumble_port() -> u16 { 64738 }
fn default_mumble_username() -> String { "SIP-Bridge".to_string() }
fn default_true() -> bool { true }
fn default_sample_rate() -> u32 { 48000 }
fn default_frame_duration() -> u32 { 20 }
fn default_opus_bitrate() -> u32 { 64000 }
fn default_jitter_buffer() -> u32 { 60 }
