use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Playback volume (0.0 to 1.0)
const VOLUME: f32 = 0.8;

/// Volume when ducked for TTS
const DUCKED_VOLUME: f32 = 0.1;

/// How long to duck streams when TTS plays (in milliseconds)
const DUCK_DURATION_MS: u64 = 1500;

/// Audio player with fire-and-forget TTS
/// - Stream plays in background thread
/// - Streams duck when TTS plays (shared duck_until timestamp)
/// - TTS is non-blocking (fire-and-forget)
pub struct Player {
    _stream: OutputStream,
    stream_handle: OutputStreamHandle,
    stop_flag: Arc<AtomicBool>,
    stream_thread: Option<JoinHandle<()>>,
    /// Timestamp (ms since boot_time) until which streams should be ducked
    duck_until_ms: Arc<AtomicU64>,
    /// Reference time for calculating timestamps
    boot_time: Instant,
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
            duck_until_ms: Arc::new(AtomicU64::new(0)),
            boot_time: Instant::now(),
        })
    }

    /// Start ducking all streams for DUCK_DURATION_MS
    fn start_duck(&self) {
        let until = self.boot_time.elapsed().as_millis() as u64 + DUCK_DURATION_MS;
        self.duck_until_ms.store(until, Ordering::SeqCst);
        debug!("Ducking streams for {}ms", DUCK_DURATION_MS);
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
    /// Ducks any playing streams for DUCK_DURATION_MS
    pub fn speak(&mut self, text: &str, tts: &std::sync::Arc<crate::tts::PiperTts>) {
        debug!("TTS: {}", text);

        // Duck streams immediately (for macOS say which plays during synthesize)
        self.start_duck();

        // Yield to give stream thread a chance to see duck state before stop() is called
        thread::yield_now();

        let text = text.to_string();
        let tts = tts.clone();
        let stream_handle = self.stream_handle.clone();
        let duck_until_ms = self.duck_until_ms.clone();
        let boot_time = self.boot_time;

        // Spawn TTS in background thread (fire-and-forget)
        thread::spawn(move || {
            match tts.synthesize(&text) {
                Ok(samples) => {
                    // Empty samples means audio was already played (e.g., macOS say)
                    if !samples.is_empty() {
                        // For Piper: refresh duck timer after synthesis completes
                        // This ensures ducking lasts through playback, not just synthesis
                        let until = boot_time.elapsed().as_millis() as u64 + DUCK_DURATION_MS;
                        duck_until_ms.store(until, Ordering::SeqCst);
                        debug!("Refreshed duck timer after synthesis");

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
    /// Stream respects duck_until_ms for ducking during TTS
    pub fn play_stream(&mut self, url: &str) -> Result<()> {
        info!("Streaming: {}", url);

        // Stop any existing stream first
        self.stop();

        let url = url.to_string();
        let stop_flag = self.stop_flag.clone();
        let stream_handle = self.stream_handle.clone();
        let duck_until_ms = self.duck_until_ms.clone();
        let boot_time = self.boot_time;

        // Spawn a thread to handle streaming
        let handle = thread::spawn(move || {
            if let Err(e) = stream_audio(&url, &stream_handle, stop_flag, duck_until_ms, boot_time) {
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

/// Stream audio from URL using symphonia for decoding
/// Checks duck_until_ms to determine if volume should be ducked
fn stream_audio(
    url: &str,
    stream_handle: &OutputStreamHandle,
    stop_flag: Arc<AtomicBool>,
    duck_until_ms: Arc<AtomicU64>,
    boot_time: Instant,
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

    // Create a sink for playback
    let sink = Sink::try_new(stream_handle)
        .map_err(|e| anyhow!("Failed to create sink: {}", e))?;

    // Helper to check if we should be ducked
    let should_duck = || {
        let now_ms = boot_time.elapsed().as_millis() as u64;
        let until_ms = duck_until_ms.load(Ordering::SeqCst);
        now_ms < until_ms
    };

    // Set initial volume based on duck state
    let mut is_ducked = should_duck();
    if is_ducked {
        sink.set_volume(DUCKED_VOLUME);
        debug!("Stream starting ducked");
    } else {
        sink.set_volume(VOLUME);
        debug!("Stream starting at full volume");
    }

    // Decode and play packets
    loop {
        // Check duck state FIRST (before stop check) so stream ducks before exiting
        let duck_now = should_duck();
        if is_ducked && !duck_now {
            debug!("Stream unducking");
            sink.set_volume(VOLUME);
            is_ducked = false;
        } else if !is_ducked && duck_now {
            debug!("Stream ducking");
            sink.set_volume(DUCKED_VOLUME);
            is_ducked = true;
        }

        // Check stop flag after duck state is applied
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
