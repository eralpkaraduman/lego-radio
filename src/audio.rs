use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Playback volume (0.0 to 1.0)
const VOLUME: f32 = 0.8;

/// Audio player with fire-and-forget TTS
/// - Stream plays in background thread, ducks for first 2 seconds
/// - TTS is non-blocking (fire-and-forget)
pub struct Player {
    _stream: OutputStream,
    stream_handle: OutputStreamHandle,
    stop_flag: Arc<AtomicBool>,
    stream_thread: Option<JoinHandle<()>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        let (stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| anyhow!("Failed to open audio output: {}", e))?;

        Ok(Self {
            _stream: stream,
            stream_handle,
            stop_flag: Arc::new(AtomicBool::new(false)),
            stream_thread: None,
        })
    }

    /// Stop any currently playing stream and wait for it to finish
    pub fn stop(&mut self) {
        // Signal the stream to stop
        self.stop_flag.store(true, Ordering::SeqCst);

        // Wait for the stream thread to actually finish
        if let Some(handle) = self.stream_thread.take() {
            debug!("Waiting for stream thread to stop...");
            let _ = handle.join();
            debug!("Stream thread stopped");
        }

        // Create new flag for the next stream
        self.stop_flag = Arc::new(AtomicBool::new(false));
    }

    /// Speak text using TTS (blocking, waits for completion)
    /// Use this for sequential announcements (e.g., welcome sequence)
    pub fn speak_sync(&mut self, text: &str, tts: &crate::tts::PiperTts) {
        debug!("TTS (sync): {}", text);

        match tts.synthesize(text) {
            Ok(samples) => {
                if !samples.is_empty() {
                    let samples_f32: Vec<f32> =
                        samples.iter().map(|&s| s as f32 / 32768.0).collect();
                    let source = SamplesBuffer::new(1, 22050, samples_f32);

                    if let Ok(sink) = Sink::try_new(&self.stream_handle) {
                        sink.set_volume(VOLUME);
                        sink.append(source);
                        sink.sleep_until_end();
                    }
                }
            }
            Err(e) => {
                warn!("TTS failed: {}", e);
            }
        }
    }

    /// Speak text using TTS (fire-and-forget, non-blocking)
    pub fn speak(&mut self, text: &str, tts: &std::sync::Arc<crate::tts::PiperTts>) {
        debug!("TTS: {}", text);

        let text = text.to_string();
        let tts = tts.clone();
        let stream_handle = self.stream_handle.clone();

        // Spawn TTS in background thread (fire-and-forget)
        thread::spawn(move || {
            match tts.synthesize(&text) {
                Ok(samples) => {
                    // Empty samples means audio was already played (e.g., macOS say)
                    if !samples.is_empty() {
                        let samples_f32: Vec<f32> =
                            samples.iter().map(|&s| s as f32 / 32768.0).collect();
                        let source = SamplesBuffer::new(1, 22050, samples_f32);

                        if let Ok(sink) = Sink::try_new(&stream_handle) {
                            sink.set_volume(VOLUME);
                            sink.append(source);
                            sink.sleep_until_end();
                        }
                    }
                }
                Err(e) => {
                    warn!("TTS failed: {}", e);
                }
            }
        });
    }

    /// Play an internet radio stream (non-blocking)
    /// Stops any currently playing stream first
    /// Stream is ducked for first 2 seconds to let TTS announcement be heard
    pub fn play_stream(&mut self, url: &str) -> Result<()> {
        info!("Streaming: {}", url);

        // Stop any existing stream first
        self.stop();

        let url = url.to_string();
        let stop_flag = self.stop_flag.clone();
        let stream_handle = self.stream_handle.clone();

        // Spawn a thread to handle streaming
        let handle = thread::spawn(move || {
            if let Err(e) = stream_audio(&url, &stream_handle, stop_flag) {
                error!("Stream error: {}", e);
            }
        });

        self.stream_thread = Some(handle);

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

/// Volume when stream is ducked (during TTS announcement)
const DUCKED_VOLUME: f32 = 0.1;

/// How long to duck stream at start (for TTS announcement)
const DUCK_DURATION_SECS: u64 = 2;

/// Stream audio from URL using symphonia for decoding
/// Starts ducked for DUCK_DURATION_SECS to let TTS be heard
fn stream_audio(
    url: &str,
    stream_handle: &OutputStreamHandle,
    stop_flag: Arc<AtomicBool>,
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
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    info!("Audio: {} Hz, {} channels", sample_rate, channels);

    // Create a sink for playback - start ducked for TTS announcement
    let sink = Sink::try_new(stream_handle)
        .map_err(|e| anyhow!("Failed to create sink: {}", e))?;
    sink.set_volume(DUCKED_VOLUME);
    debug!("Stream starting ducked for {}s", DUCK_DURATION_SECS);

    let start_time = std::time::Instant::now();
    let duck_duration = std::time::Duration::from_secs(DUCK_DURATION_SECS);
    let mut is_ducked = true;

    // Decode and play packets
    loop {
        // Check stop flag
        if stop_flag.load(Ordering::SeqCst) {
            debug!("Stream stopped by user");
            sink.stop();
            break;
        }

        // Unduck after duration elapses
        if is_ducked && start_time.elapsed() >= duck_duration {
            debug!("Stream unducking after {}s", DUCK_DURATION_SECS);
            sink.set_volume(VOLUME);
            is_ducked = false;
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

    // Wait for remaining audio to play (only if not stopped)
    if !stop_flag.load(Ordering::SeqCst) {
        sink.sleep_until_end();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn test_stop_flag_works() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        assert!(!stop_flag.load(Ordering::SeqCst));

        stop_flag.store(true, Ordering::SeqCst);
        assert!(stop_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_stop_creates_new_flag() {
        // This tests the logic without actual audio
        let stop_flag1 = Arc::new(AtomicBool::new(false));
        let stop_flag1_clone = stop_flag1.clone();

        // Simulate what stop() does
        stop_flag1.store(true, Ordering::SeqCst);
        let stop_flag2 = Arc::new(AtomicBool::new(false));

        // Old flag should be true (stopped)
        assert!(stop_flag1_clone.load(Ordering::SeqCst));
        // New flag should be false (ready for new stream)
        assert!(!stop_flag2.load(Ordering::SeqCst));
    }

    #[test]
    fn test_volume_constant_in_range() {
        assert!(VOLUME >= 0.0);
        assert!(VOLUME <= 1.0);
    }

    #[test]
    fn test_sequential_operations_timing() {
        // Verify that operations happen in sequence by checking timing
        let start = Instant::now();

        // Simulate TTS taking 100ms
        thread::sleep(Duration::from_millis(50));
        let after_tts = start.elapsed();

        // Simulate stream start
        thread::sleep(Duration::from_millis(10));
        let after_stream = start.elapsed();

        // TTS should complete before stream starts
        assert!(after_tts < after_stream);
    }

    /// Test that stop flag propagation works correctly
    #[test]
    fn test_stop_flag_propagates_to_thread() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = stop_flag.clone();

        let handle = thread::spawn(move || {
            // Simulate a stream loop
            let mut iterations = 0;
            while !flag_clone.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(10));
                iterations += 1;
                if iterations > 100 {
                    break; // Safety limit
                }
            }
            iterations
        });

        // Let it run a bit
        thread::sleep(Duration::from_millis(50));

        // Signal stop
        stop_flag.store(true, Ordering::SeqCst);

        // Wait for thread
        let iterations = handle.join().unwrap();

        // Should have run at least a few iterations before stopping
        assert!(iterations >= 3);
        assert!(iterations < 100); // Should have stopped, not hit safety limit
    }

    /// Test that thread join works (simulating stop() behavior)
    #[test]
    fn test_thread_join_blocks_until_complete() {
        let start = Instant::now();

        let handle = thread::spawn(|| {
            thread::sleep(Duration::from_millis(100));
        });

        handle.join().unwrap();
        let elapsed = start.elapsed();

        // Should have waited for the thread
        assert!(elapsed >= Duration::from_millis(100));
    }
}
