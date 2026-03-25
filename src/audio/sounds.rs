use std::sync::OnceLock;
use std::time::Duration;

const CONNECTED_WAV: &[u8] = include_bytes!("../../sounds/ServerConnected.wav");
const USER_JOINED_WAV: &[u8] = include_bytes!("../../sounds/UserJoinedChannel.wav");
const USER_LEFT_WAV: &[u8] = include_bytes!("../../sounds/UserLeftChannel.wav");
const TEXT_MESSAGE_WAV: &[u8] = include_bytes!("../../sounds/TextMessage.wav");
const INTRO_WAV: &[u8] = include_bytes!("../../sounds/intro.wav");

static CONNECTED_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static USER_JOINED_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static USER_LEFT_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static TEXT_MESSAGE_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static INTRO_PCM: OnceLock<Vec<i16>> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub enum SoundEvent {
    SelfJoinedChannel,
    UserJoinedChannel,
    UserLeftChannel,
    TextMessage,
    Intro,
}

/// Get a static reference to the pre-decoded 48kHz mono PCM for a sound event.
/// Decoded on first call, cached thereafter.
fn get_sound_ref(event: SoundEvent) -> &'static [i16] {
    match event {
        SoundEvent::SelfJoinedChannel => {
            CONNECTED_PCM.get_or_init(|| decode_wav_48k(CONNECTED_WAV))
        }
        SoundEvent::UserJoinedChannel => {
            USER_JOINED_PCM.get_or_init(|| decode_wav_48k(USER_JOINED_WAV))
        }
        SoundEvent::UserLeftChannel => USER_LEFT_PCM.get_or_init(|| decode_wav_48k(USER_LEFT_WAV)),
        SoundEvent::TextMessage => {
            TEXT_MESSAGE_PCM.get_or_init(|| decode_wav_48k(TEXT_MESSAGE_WAV))
        }
        SoundEvent::Intro => INTRO_PCM.get_or_init(|| decode_wav_48k(INTRO_WAV)),
    }
}

/// Get a cloned copy of the pre-decoded 48kHz mono PCM for a sound event.
pub fn get_sound(event: SoundEvent) -> Vec<i16> {
    get_sound_ref(event).to_vec()
}

/// Compute the playout duration of a sound effect at the given sample rate.
///
/// Sound PCM is always stored at 48kHz, but the mixer drains samples at
/// `sample_rate` per second, so the actual wall-clock duration depends on
/// the configured rate, not the stored rate.
pub fn sound_duration(event: SoundEvent, sample_rate: u32) -> Duration {
    Duration::from_secs_f64(get_sound_ref(event).len() as f64 / sample_rate as f64)
}

/// Parse a 16-bit mono PCM WAV and return 48kHz samples.
///
/// Resamples via linear interpolation if the source rate differs from 48kHz.
/// Panics if the embedded WAV is malformed (build-time guarantee).
fn decode_wav_48k(wav_bytes: &[u8]) -> Vec<i16> {
    assert!(wav_bytes.len() >= 44, "WAV too short");
    assert!(&wav_bytes[0..4] == b"RIFF", "Not a RIFF file");
    assert!(&wav_bytes[8..12] == b"WAVE", "Not a WAVE file");

    let audio_format = u16::from_le_bytes(wav_bytes[20..22].try_into().unwrap());
    let channels = u16::from_le_bytes(wav_bytes[22..24].try_into().unwrap());
    let sample_rate = u32::from_le_bytes(wav_bytes[24..28].try_into().unwrap());
    let bits_per_sample = u16::from_le_bytes(wav_bytes[34..36].try_into().unwrap());

    assert_eq!(audio_format, 1, "Only PCM WAV supported");
    assert_eq!(channels, 1, "Only mono WAV supported");
    assert!(sample_rate > 0, "Invalid sample rate");
    assert_eq!(bits_per_sample, 16, "Only 16-bit WAV supported");

    let pcm_bytes = &wav_bytes[44..];
    let samples: Vec<i16> = pcm_bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();

    if sample_rate == 48000 {
        return samples;
    }

    // Linear interpolation resample to 48kHz
    let ratio = sample_rate as f64 / 48000.0;
    let out_len = ((samples.len() as f64) / ratio).ceil() as usize;
    (0..out_len)
        .map(|i| {
            let src_pos = i as f64 * ratio;
            let idx = src_pos as usize;
            let frac = src_pos - idx as f64;
            let a = samples[idx] as f64;
            let b = samples.get(idx + 1).copied().unwrap_or(samples[idx]) as f64;
            (a + frac * (b - a)).round() as i16
        })
        .collect()
}
