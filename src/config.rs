use serde::Deserialize;

use crate::mumble::control::MumbleConfig;
use crate::sip::SipConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub sip: SipSection,
    pub mumble: MumbleSection,
    #[serde(default)]
    pub audio: AudioSection,
    #[serde(default)]
    pub tts: TtsSection,
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

#[derive(Clone, Debug, Deserialize)]
pub struct TtsSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_tts_host")]
    pub host: String,
    #[serde(default = "default_tts_port")]
    pub port: u16,
    #[serde(default = "default_tts_voice")]
    pub voice: String,
    #[serde(default = "default_true")]
    pub announce_on_connect: bool,
    #[serde(default = "default_tts_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    #[serde(default = "default_tts_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_tts_announcement_debounce_ms")]
    pub announcement_debounce_ms: u64,
    #[serde(default = "default_true")]
    pub auto_restart: bool,
}

impl Default for TtsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_tts_host(),
            port: default_tts_port(),
            voice: default_tts_voice(),
            announce_on_connect: default_true(),
            startup_timeout_ms: default_tts_startup_timeout_ms(),
            request_timeout_ms: default_tts_request_timeout_ms(),
            announcement_debounce_ms: default_tts_announcement_debounce_ms(),
            auto_restart: default_true(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.sip.validate()?;
        Ok(())
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

impl SipSection {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_concurrent_calls >= 1,
            "sip.max_concurrent_calls must be 1 or greater (got {})",
            self.max_concurrent_calls
        );
        Ok(())
    }
}

fn default_sip_port() -> u16 {
    5060
}
fn default_max_calls() -> u32 {
    10
}
fn default_mumble_port() -> u16 {
    64738
}
fn default_mumble_username() -> String {
    "SIP-Bridge".to_string()
}
fn default_true() -> bool {
    true
}
fn default_sample_rate() -> u32 {
    48000
}
fn default_frame_duration() -> u32 {
    10
}
fn default_opus_bitrate() -> u32 {
    64000
}
fn default_jitter_buffer() -> u32 {
    60
}
fn default_tts_host() -> String {
    "127.0.0.1".to_string()
}
fn default_tts_port() -> u16 {
    8000
}
fn default_tts_voice() -> String {
    "eponine".to_string()
}
fn default_tts_startup_timeout_ms() -> u64 {
    20_000
}
fn default_tts_request_timeout_ms() -> u64 {
    3_000
}
fn default_tts_announcement_debounce_ms() -> u64 {
    750
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn parses_tts_defaults_when_section_missing() {
        let parsed: Config = toml::from_str(
            r#"
[sip]
listen_port = 5060
account_uri = "sip:bridge@pbx.example.com"

[mumble]
default_host = "mumble.example.com"
"#,
        )
        .expect("config should parse");

        assert!(!parsed.tts.enabled);
        assert_eq!(parsed.tts.host, "127.0.0.1");
        assert_eq!(parsed.tts.port, 8000);
        assert_eq!(parsed.tts.voice, "eponine");
        assert!(parsed.tts.announce_on_connect);
        assert_eq!(parsed.tts.startup_timeout_ms, 20_000);
        assert_eq!(parsed.tts.request_timeout_ms, 3_000);
        assert_eq!(parsed.tts.announcement_debounce_ms, 750);
        assert!(parsed.tts.auto_restart);
    }

    #[test]
    fn parses_tts_overrides() {
        let parsed: Config = toml::from_str(
            r#"
[sip]
listen_port = 5060
account_uri = "sip:bridge@pbx.example.com"

[mumble]
default_host = "mumble.example.com"

[tts]
enabled = true
host = "0.0.0.0"
port = 9000
voice = "alice"
announce_on_connect = false
startup_timeout_ms = 5000
request_timeout_ms = 1500
announcement_debounce_ms = 400
auto_restart = false
"#,
        )
        .expect("config should parse");

        assert!(parsed.tts.enabled);
        assert_eq!(parsed.tts.host, "0.0.0.0");
        assert_eq!(parsed.tts.port, 9000);
        assert_eq!(parsed.tts.voice, "alice");
        assert!(!parsed.tts.announce_on_connect);
        assert_eq!(parsed.tts.startup_timeout_ms, 5_000);
        assert_eq!(parsed.tts.request_timeout_ms, 1_500);
        assert_eq!(parsed.tts.announcement_debounce_ms, 400);
        assert!(!parsed.tts.auto_restart);
    }

    #[test]
    fn validate_rejects_zero_max_concurrent_calls() {
        let parsed: Config = toml::from_str(
            r#"
[sip]
listen_port = 5060
account_uri = "sip:bridge@pbx.example.com"
max_concurrent_calls = 0

[mumble]
default_host = "mumble.example.com"
"#,
        )
        .expect("config should parse");

        assert!(parsed.validate().is_err());
    }

    #[test]
    fn validate_accepts_positive_max_concurrent_calls() {
        let parsed: Config = toml::from_str(
            r#"
[sip]
listen_port = 5060
account_uri = "sip:bridge@pbx.example.com"
max_concurrent_calls = 1

[mumble]
default_host = "mumble.example.com"
"#,
        )
        .expect("config should parse");

        parsed.validate().expect("config should validate");
    }
}
