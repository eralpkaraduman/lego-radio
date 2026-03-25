use serde::Deserialize;
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Deserialize)]
struct RadioConfig {
    output_dir: String,
    channels: Vec<Channel>,
    ui: Vec<TtsEntry>,
}

#[derive(Deserialize)]
struct Channel {
    name: String,
    url: String,
    text: String,
    #[allow(dead_code)]
    voice: String,
    file: String,
}

#[derive(Deserialize)]
struct TtsEntry {
    text: String,
    #[allow(dead_code)]
    voice: String,
    file: String,
}

fn main() {
    println!("cargo:rerun-if-changed=radio.toml");
    println!("cargo:rerun-if-changed=audio");

    let out_dir = std::env::var("OUT_DIR").unwrap();

    let toml_str = match fs::read_to_string("radio.toml") {
        Ok(c) => c,
        Err(_) => {
            // Not available during dummy dep-cache build
            let p = Path::new(&out_dir).join("tts_generated.rs");
            let mut f = fs::File::create(p).unwrap();
            writeln!(f, "pub fn get_audio(_: &str) -> Option<&'static [u8]> {{ None }}").unwrap();

            let p = Path::new(&out_dir).join("channels_generated.rs");
            let mut f = fs::File::create(p).unwrap();
            writeln!(f, "pub const CHANNELS: &[Channel] = &[];").unwrap();
            return;
        }
    };

    let config: RadioConfig = toml::from_str(&toml_str).expect("Failed to parse radio.toml");

    // Generate TTS audio lookup (channels + ui phrases)
    let tts_path = Path::new(&out_dir).join("tts_generated.rs");
    let mut f = fs::File::create(tts_path).unwrap();

    writeln!(f, "/// Auto-generated from radio.toml — do not edit").unwrap();
    writeln!(f, "pub fn get_audio(phrase: &str) -> Option<&'static [u8]> {{").unwrap();
    writeln!(f, "    match phrase {{").unwrap();

    for ch in &config.channels {
        let path = format!("{}/{}.raw", config.output_dir, ch.file);
        writeln!(
            f,
            "        {:?} => Some(include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{}\"))),",
            ch.text, path
        )
        .unwrap();
    }
    for ui in &config.ui {
        let path = format!("{}/{}.raw", config.output_dir, ui.file);
        writeln!(
            f,
            "        {:?} => Some(include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/{}\"))),",
            ui.text, path
        )
        .unwrap();
    }

    writeln!(f, "        _ => None,").unwrap();
    writeln!(f, "    }}").unwrap();
    writeln!(f, "}}").unwrap();

    // Generate channel list
    let ch_path = Path::new(&out_dir).join("channels_generated.rs");
    let mut f = fs::File::create(ch_path).unwrap();

    writeln!(f, "/// Auto-generated from radio.toml — do not edit").unwrap();
    writeln!(f, "pub const CHANNELS: &[Channel] = &[").unwrap();
    for ch in &config.channels {
        writeln!(f, "    Channel {{").unwrap();
        writeln!(f, "        name: {:?},", ch.name).unwrap();
        writeln!(f, "        tts_name: {:?},", ch.text).unwrap();
        writeln!(f, "        url: {:?},", ch.url).unwrap();
        writeln!(f, "    }},").unwrap();
    }
    writeln!(f, "];").unwrap();
}
