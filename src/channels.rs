/// Radio channel definition
#[derive(Debug, Clone)]
pub struct Channel {
    pub name: &'static str,
    /// TTS-friendly name (avoids abbreviations)
    pub tts_name: &'static str,
    pub url: &'static str,
}

// Channel list auto-generated from radio.toml by build.rs
include!(concat!(env!("OUT_DIR"), "/channels_generated.rs"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::const_is_empty)]
    fn test_channels_not_empty() {
        assert!(!CHANNELS.is_empty(), "Must have at least one channel");
    }

    #[test]
    fn test_channels_have_valid_urls() {
        for channel in CHANNELS {
            assert!(
                channel.url.starts_with("http://") || channel.url.starts_with("https://"),
                "Channel '{}' has invalid URL: {}",
                channel.name,
                channel.url
            );
        }
    }

    #[test]
    fn test_channels_have_names() {
        for channel in CHANNELS {
            assert!(!channel.name.is_empty(), "Channel must have a name");
            assert!(!channel.tts_name.is_empty(), "Channel must have a TTS name");
        }
    }

    #[test]
    fn test_tts_audio_exists_for_all_channels() {
        for channel in CHANNELS {
            assert!(
                crate::tts::get_audio(channel.tts_name).is_some(),
                "Missing TTS audio for channel: {}",
                channel.name
            );
        }
    }
}
