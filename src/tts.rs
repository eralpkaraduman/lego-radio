/// Pre-generated TTS audio lookup
///
/// Audio files are generated at build time by Piper TTS and embedded
/// in the binary via `include_bytes!`. The phrase-to-file mapping is
/// auto-generated from `radio.toml` by `build.rs`.
///
/// To regenerate audio: `docker build --output=out .`

/// Raw PCM audio: 16-bit signed little-endian, 22050 Hz, mono
pub const SAMPLE_RATE: u32 = 22050;

// Include the auto-generated lookup function from build.rs
include!(concat!(env!("OUT_DIR"), "/tts_generated.rs"));

/// Convert raw PCM bytes (i16 LE) to f32 samples for rodio playback
pub fn pcm_to_f32(raw: &[u8]) -> Vec<f32> {
    raw.chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / 32768.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_phrases_from_toml() {
        // Extract text values from radio.toml and verify audio exists for each
        let toml = include_str!("../radio.toml");
        for line in toml.lines() {
            let line = line.trim();
            if let Some(text) = line.strip_prefix("text = \"") {
                let text = text.trim_end_matches('"');
                assert!(
                    get_audio(text).is_some(),
                    "Missing audio for phrase: {:?}",
                    text
                );
            }
        }
    }

    #[test]
    fn test_unknown_phrase_returns_none() {
        assert!(get_audio("nonexistent phrase").is_none());
    }

    #[test]
    fn test_pcm_to_f32_conversion() {
        let raw = [0xFF, 0x7F]; // 32767 in LE
        let samples = pcm_to_f32(&raw);
        assert!((samples[0] - 0.99997).abs() < 0.001);

        let raw = [0x00, 0x80]; // -32768 in LE
        let samples = pcm_to_f32(&raw);
        assert!((samples[0] - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_audio_data_not_empty() {
        for phrase in ["Hello!", "Ready.", "Radio off", "Off"] {
            let audio = get_audio(phrase).unwrap();
            assert!(!audio.is_empty(), "Audio for '{}' is empty", phrase);
            assert!(
                audio.len() > 100,
                "Audio for '{}' suspiciously small: {} bytes",
                phrase,
                audio.len()
            );
        }
    }
}
