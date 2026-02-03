use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Audio player that handles TTS and streaming
pub struct Player {
    _stream: OutputStream,
    stream_handle: OutputStreamHandle,
    stop_flag: Arc<AtomicBool>,
    volume: f32,
}

impl Player {
    pub fn new() -> Result<Self> {
        let (stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| anyhow!("Failed to open audio output: {}", e))?;

        Ok(Self {
            _stream: stream,
            stream_handle,
            stop_flag: Arc::new(AtomicBool::new(false)),
            volume: 1.0,
        })
    }

    /// Set playback volume (0.0 to 1.0)
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        info!("Volume set to {}%", (self.volume * 100.0) as i32);
    }


    /// Stop any currently playing audio
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        // Create a new stop flag for the next stream
        self.stop_flag = Arc::new(AtomicBool::new(false));
    }

    /// Speak text using Piper TTS
    pub fn speak(&self, text: &str, tts: &crate::tts::PiperTts) {
        debug!("TTS: {}", text);

        match tts.synthesize(text) {
            Ok(samples) => {
                // Convert i16 to f32 samples for rodio
                let samples_f32: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

                // Piper outputs 22050 Hz mono audio
                let source = SamplesBuffer::new(1, 22050, samples_f32);

                if let Ok(sink) = Sink::try_new(&self.stream_handle) {
                    sink.set_volume(self.volume);
                    sink.append(source);
                    sink.sleep_until_end();
                }
            }
            Err(e) => {
                error!("TTS error: {}", e);
            }
        }
    }

    /// Play an internet radio stream
    pub fn play_stream(&mut self, url: &str) -> Result<()> {
        info!("Streaming: {}", url);

        // Reset stop flag
        self.stop_flag.store(false, Ordering::SeqCst);

        // Clone for the thread
        let url = url.to_string();
        let stop_flag = self.stop_flag.clone();
        let stream_handle = self.stream_handle.clone();
        let volume = self.volume;

        // Spawn a thread to handle streaming
        thread::spawn(move || {
            if let Err(e) = stream_audio(&url, &stream_handle, stop_flag, volume) {
                error!("Stream error: {}", e);
            }
        });

        Ok(())
    }
}

/// HTTP streaming reader that implements MediaSource
struct HttpStreamReader {
    reader: BufReader<Box<dyn Read + Send + Sync>>,
}

impl std::io::Read for HttpStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl std::io::Seek for HttpStreamReader {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        // Streams are not seekable, but we need to implement the trait
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Seeking not supported for streams",
        ))
    }
}

impl symphonia::core::io::MediaSource for HttpStreamReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Stream audio from URL using symphonia for decoding
fn stream_audio(
    url: &str,
    stream_handle: &OutputStreamHandle,
    stop_flag: Arc<AtomicBool>,
    volume: f32,
) -> Result<()> {
    // Make HTTP request
    let response = ureq::get(url)
        .set("User-Agent", "lego-radio/1.0")
        .set("Icy-MetaData", "0")
        .call()
        .map_err(|e| anyhow!("HTTP request failed: {}", e))?;

    let content_type = response.content_type().to_string();
    debug!("Content-Type: {}", content_type);

    // Create media source from HTTP stream
    let reader = HttpStreamReader {
        reader: BufReader::with_capacity(64 * 1024, response.into_reader()),
    };

    let mss = MediaSourceStream::new(Box::new(reader), Default::default());

    // Create a hint based on content type
    let mut hint = Hint::new();
    if content_type.contains("audio/mpeg") || content_type.contains("audio/mp3") {
        hint.with_extension("mp3");
    } else if content_type.contains("audio/aac") || content_type.contains("audio/aacp") {
        hint.with_extension("aac");
    }

    // Probe the format
    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let metadata_opts = MetadataOptions::default();

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &format_opts, &metadata_opts)
        .map_err(|e| anyhow!("Failed to probe format: {}", e))?;

    let mut format = probed.format;

    // Get the default track
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No audio track found"))?;

    let track_id = track.id;

    // Create decoder
    let dec_opts = DecoderOptions::default();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &dec_opts)
        .map_err(|e| anyhow!("Failed to create decoder: {}", e))?;

    // Get audio parameters
    let sample_rate = track
        .codec_params
        .sample_rate
        .unwrap_or(44100);
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(2);

    info!("Audio: {} Hz, {} channels", sample_rate, channels);

    // Create a sink for playback
    let sink = Sink::try_new(stream_handle)
        .map_err(|e| anyhow!("Failed to create sink: {}", e))?;
    sink.set_volume(volume);

    // Decode and play packets
    loop {
        // Check stop flag
        if stop_flag.load(Ordering::SeqCst) {
            debug!("Stream stopped by user");
            sink.stop();
            break;
        }

        // Read next packet
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                warn!("Stream ended");
                break;
            }
            Err(e) => {
                error!("Error reading packet: {}", e);
                break;
            }
        };

        // Skip packets from other tracks
        if packet.track_id() != track_id {
            continue;
        }

        // Decode the packet
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(e) => {
                warn!("Decode error: {}", e);
                continue;
            }
        };

        // Convert to samples
        let spec = *decoded.spec();
        let duration = decoded.capacity() as u64;

        let mut sample_buf = SampleBuffer::<f32>::new(duration, spec);
        sample_buf.copy_interleaved_ref(decoded);

        let samples = sample_buf.samples().to_vec();

        // Play through rodio
        let source = SamplesBuffer::new(channels as u16, sample_rate, samples);
        sink.append(source);
    }

    // Wait for remaining audio to play
    if !stop_flag.load(Ordering::SeqCst) {
        sink.sleep_until_end();
    }

    Ok(())
}
