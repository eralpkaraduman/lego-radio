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
    // BBC Radio (HLS streams)
    Channel {
        name: "BBC Radio 1",
        tts_name: "B B C Radio 1",
        url: "http://as-hls-ww-live.akamaized.net/pool_01505109/live/ww/bbc_radio_one/bbc_radio_one.isml/bbc_radio_one-audio=96000.norewind.m3u8",
    },
    Channel {
        name: "BBC Radio 2",
        tts_name: "B B C Radio 2",
        url: "http://as-hls-ww-live.akamaized.net/pool_74208725/live/ww/bbc_radio_two/bbc_radio_two.isml/bbc_radio_two-audio=96000.norewind.m3u8",
    },
    Channel {
        name: "BBC Radio 3",
        tts_name: "B B C Radio 3",
        url: "http://as-hls-ww-live.akamaized.net/pool_23461179/live/ww/bbc_radio_three/bbc_radio_three.isml/bbc_radio_three-audio=96000.norewind.m3u8",
    },
    Channel {
        name: "BBC Radio 4",
        tts_name: "B B C Radio 4",
        url: "http://as-hls-ww-live.akamaized.net/pool_55057080/live/ww/bbc_radio_fourfm/bbc_radio_fourfm.isml/bbc_radio_fourfm-audio=128000.norewind.m3u8",
    },
];

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
