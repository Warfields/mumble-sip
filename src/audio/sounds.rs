use std::sync::OnceLock;

const CONNECTED_WAV: &[u8] = include_bytes!("../../sounds/ServerConnected.wav");
const USER_JOINED_WAV: &[u8] = include_bytes!("../../sounds/UserJoinedChannel.wav");
const USER_LEFT_WAV: &[u8] = include_bytes!("../../sounds/UserLeftChannel.wav");
const TEXT_MESSAGE_WAV: &[u8] = include_bytes!("../../sounds/TextMessage.wav");

static CONNECTED_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static USER_JOINED_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static USER_LEFT_PCM: OnceLock<Vec<i16>> = OnceLock::new();
static TEXT_MESSAGE_PCM: OnceLock<Vec<i16>> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub enum SoundEvent {
    SelfJoinedChannel,
    UserJoinedChannel,
    UserLeftChannel,
    TextMessage,
}

/// Get the pre-decoded 48kHz mono PCM for a sound event.
/// Decoded on first call, cached thereafter.
pub fn get_sound(event: SoundEvent) -> Vec<i16> {
    let pcm = match event {
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
    };
    pcm.clone()
}

/// Parse a 48kHz 16-bit mono PCM WAV and return samples.
///
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
    assert_eq!(sample_rate, 48000, "Only 48kHz WAV supported");
    assert_eq!(bits_per_sample, 16, "Only 16-bit WAV supported");

    let pcm_bytes = &wav_bytes[44..];
    pcm_bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}
