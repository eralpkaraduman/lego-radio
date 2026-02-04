/// Radio channel definition
#[derive(Debug, Clone)]
pub struct Channel {
    pub name: &'static str,
    /// TTS-friendly name (avoids abbreviations)
    pub tts_name: &'static str,
    pub url: &'static str,
}

/// Hardcoded list of radio channels
/// Edit this list and release a new version to change channels
pub const CHANNELS: &[Channel] = &[
    Channel {
        name: "YLE Klassinen",
        tts_name: "Y L E Classical",
        url: "https://icecast.live.yle.fi/radio/YleKlassinen/icecast.audio",
    },
    Channel {
        name: "YLE Radio 1",
        tts_name: "Y L E Radio 1",
        url: "https://icecast.live.yle.fi/radio/YleRadio1/icecast.audio",
    },
    Channel {
        name: "YLE Radio Suomi",
        tts_name: "Y L E Radio Suomi",
        url: "https://icecast.live.yle.fi/radio/YleRS/icecast.audio",
    },
    Channel {
        name: "YleX",
        tts_name: "Y L E X",
        url: "https://icecast.live.yle.fi/radio/YleX/icecast.audio",
    },
    Channel {
        name: "Soma FM Groove Salad",
        tts_name: "Soma Groove Salad",
        url: "https://ice1.somafm.com/groovesalad-128-mp3",
    },
    Channel {
        name: "Soma FM Indie Pop",
        tts_name: "Soma Indie Pop Rocks",
        url: "https://ice1.somafm.com/indiepop-128-mp3",
    },
    Channel {
        name: "Soma FM Secret Agent",
        tts_name: "Soma Secret Agent",
        url: "https://ice1.somafm.com/secretagent-128-mp3",
    },
    Channel {
        name: "Radyo Eksen",
        tts_name: "Radyo Eksen",
        url: "http://eksenwmp.radyotvonline.com/;stream.mp3",
    },
    Channel {
        name: "BBC World Service",
        tts_name: "B B C World Service",
        url: "http://stream.live.vc.bbcmedia.co.uk/bbc_world_service",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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
    fn test_tts_names_readable() {
        for channel in CHANNELS {
            // TTS names should be readable (no compressed abbreviations)
            assert!(
                !channel.tts_name.contains("YLE") && !channel.tts_name.contains("FM"),
                "TTS name should spell out abbreviations: {}",
                channel.tts_name
            );
        }
    }
}
