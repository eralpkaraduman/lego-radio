use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
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

/// Volume for muted/inactive streams
const MUTED_VOLUME: f32 = 0.0;

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

    /// Extend duck timer to survive HTTP connect delays
    /// Called by play_stream() after stop() to ensure new stream starts ducked
    fn extend_duck_for_new_stream(&self) {
        let now_ms = self.boot_time.elapsed().as_millis() as u64;
        let until_ms = self.duck_until_ms.load(Ordering::SeqCst);

        // If duck was recently requested (active or expired within 2s), extend it
        // This handles slow HTTP connects that would otherwise miss the duck window
        let duck_active = now_ms < until_ms;
        let duck_recent = until_ms > 0 && now_ms.saturating_sub(until_ms) < 2000;

        if duck_active || duck_recent {
            // Extend by 5 seconds to cover worst-case HTTP connect time
            let extended = now_ms + 5000;
            self.duck_until_ms.store(extended, Ordering::SeqCst);
            debug!(
                "Extended duck timer for new stream (was {}ms, now {}ms, until {}ms)",
                until_ms, now_ms, extended
            );
        }
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

        // Extend duck timer to survive HTTP connect delays
        // This ensures new stream starts ducked even if HTTP is slow
        self.extend_duck_for_new_stream();

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

// =============================================================================
// Multi-Stream Player (all channels playing simultaneously, one audible)
// =============================================================================

/// Stream status values
const STREAM_STATUS_CONNECTING: u8 = 0;
const STREAM_STATUS_PLAYING: u8 = 1;
const STREAM_STATUS_ERROR: u8 = 2;

/// Handle for a single stream in the multi-stream player
struct StreamHandle {
    thread: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
    /// Sink wrapped for thread-safe volume control from main thread
    sink: Arc<Mutex<Option<Arc<Sink>>>>,
    /// Stream status: 0=connecting, 1=playing, 2=error
    status: Arc<AtomicU8>,
}

/// Multi-stream player - all channels playing simultaneously, one audible
///
/// Architecture:
/// - All 4 streams connect at boot via connect_all()
/// - All streams decode and play continuously (but muted)
/// - select(idx) makes one stream audible (instant switch)
/// - disconnect_all() stops all streams (Off state, saves bandwidth)
pub struct MultiStreamPlayer {
    _stream: OutputStream,
    stream_handle: OutputStreamHandle,
    /// Array of stream handles, one per channel
    streams: [Option<StreamHandle>; 4],
    /// Currently active (audible) stream index, None = all muted
    active_index: Option<usize>,
}

impl MultiStreamPlayer {
    /// Create a new multi-stream player
    pub fn new() -> Result<Self> {
        let (stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| anyhow!("Failed to open audio output: {}", e))?;

        Ok(Self {
            _stream: stream,
            stream_handle,
            streams: [None, None, None, None],
            active_index: None,
        })
    }

    /// Switch to channel (instant - just volume change)
    /// Sets the specified stream to audible volume, mutes all others
    pub fn select(&mut self, index: usize) {
        debug!("Selecting channel {}", index);

        for (i, stream) in self.streams.iter().enumerate() {
            if let Some(ref sh) = stream {
                if let Ok(sink_guard) = sh.sink.lock() {
                    if let Some(ref sink) = *sink_guard {
                        let volume = if i == index { VOLUME } else { MUTED_VOLUME };
                        sink.set_volume(volume);
                    }
                }
            }
        }
        self.active_index = Some(index);
    }

    /// Mute the active stream (for TTS or Off state)
    pub fn mute_active(&self) {
        if let Some(idx) = self.active_index {
            if let Some(ref sh) = self.streams[idx] {
                if let Ok(sink_guard) = sh.sink.lock() {
                    if let Some(ref sink) = *sink_guard {
                        sink.set_volume(MUTED_VOLUME);
                        debug!("Muted active stream {}", idx);
                    }
                }
            }
        }
    }

    /// Speak text using TTS (blocking, waits for completion)
    /// Use this for sequential announcements (e.g., welcome sequence)
    pub fn speak_sync(&self, text: &str, tts: &crate::tts::PiperTts) {
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
    /// Mutes active stream during TTS, unmutes after
    pub fn speak(&self, text: &str, tts: &std::sync::Arc<crate::tts::PiperTts>) {
        debug!("TTS: {}", text);

        // Mute active stream immediately
        self.mute_active();

        // Get reference to active stream's sink for unmuting later
        let active_sink = self.active_index.and_then(|idx| {
            self.streams[idx].as_ref().map(|sh| sh.sink.clone())
        });

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

            // Unmute the stream after TTS completes
            if let Some(sink_mutex) = active_sink {
                if let Ok(sink_guard) = sink_mutex.lock() {
                    if let Some(ref sink) = *sink_guard {
                        sink.set_volume(VOLUME);
                        debug!("Unmuted stream after TTS");
                    }
                }
            }
        });
    }

    /// Get the status of a stream
    #[allow(dead_code)]
    pub fn stream_status(&self, index: usize) -> Option<u8> {
        self.streams.get(index)?.as_ref().map(|sh| sh.status.load(Ordering::SeqCst))
    }

    /// Check if a specific stream is connected and playing
    #[allow(dead_code)]
    pub fn is_stream_playing(&self, index: usize) -> bool {
        self.stream_status(index) == Some(STREAM_STATUS_PLAYING)
    }

    /// Connect all streams at boot (blocking, with timeout)
    /// Returns number of successfully connected streams
    pub fn connect_all(&mut self, channels: &[crate::channels::Channel], timeout: Duration) -> usize {
        info!("Connecting to {} stations (timeout: {:?})...", channels.len(), timeout);

        let (tx, rx) = mpsc::channel();
        let deadline = Instant::now() + timeout;

        // Spawn connection threads in parallel
        for (i, channel) in channels.iter().enumerate() {
            let tx = tx.clone();
            let url = channel.url.to_string();
            let stream_handle = self.stream_handle.clone();

            thread::spawn(move || {
                debug!("Connecting stream {}: {}", i, url);
                let result = connect_single_stream(i, &url, stream_handle);
                let _ = tx.send((i, result));
            });
        }
        drop(tx); // Close sender so rx knows when all done

        // Collect results with timeout
        let mut connected = 0;
        for _ in 0..channels.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!("Connection timeout reached");
                break;
            }

            match rx.recv_timeout(remaining) {
                Ok((i, Ok(handle))) => {
                    info!("Stream {} connected", i);
                    self.streams[i] = Some(handle);
                    connected += 1;
                }
                Ok((i, Err(e))) => {
                    warn!("Failed to connect stream {}: {}", i, e);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    warn!("Connection timeout");
                    break;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // All senders dropped, we're done
                    break;
                }
            }
        }

        info!("Connected {} out of {} streams", connected, channels.len());
        connected
    }

    /// Disconnect all streams (Off state)
    /// Saves bandwidth when radio is not in use
    pub fn disconnect_all(&mut self) {
        info!("Disconnecting all streams");

        // Signal all streams to stop
        for stream in self.streams.iter() {
            if let Some(ref sh) = stream {
                sh.stop_flag.store(true, Ordering::SeqCst);
            }
        }

        // Wait for all threads to finish
        for stream in self.streams.iter_mut() {
            if let Some(sh) = stream.take() {
                debug!("Waiting for stream thread to stop...");
                let _ = sh.thread.join();
            }
        }

        self.active_index = None;
        debug!("All streams disconnected");
    }

    /// Check if active stream has an error
    /// Returns true if the active stream is in error state
    pub fn active_stream_has_error(&self) -> bool {
        if let Some(idx) = self.active_index {
            self.stream_status(idx) == Some(STREAM_STATUS_ERROR)
        } else {
            false
        }
    }

    /// Reconnect a single stream that has failed
    /// Returns true if reconnection was successful
    pub fn reconnect_stream(&mut self, index: usize, channel: &crate::channels::Channel) -> bool {
        info!("Reconnecting stream {}: {}", index, channel.name);

        // Stop existing stream if any
        if let Some(sh) = self.streams[index].take() {
            sh.stop_flag.store(true, Ordering::SeqCst);
            let _ = sh.thread.join();
        }

        // Try to reconnect
        match connect_single_stream(index, channel.url, self.stream_handle.clone()) {
            Ok(handle) => {
                // Set volume based on whether this is the active stream
                if let Ok(sink_guard) = handle.sink.lock() {
                    if let Some(ref sink) = *sink_guard {
                        let volume = if self.active_index == Some(index) {
                            VOLUME
                        } else {
                            MUTED_VOLUME
                        };
                        sink.set_volume(volume);
                    }
                }
                self.streams[index] = Some(handle);
                info!("Stream {} reconnected successfully", index);
                true
            }
            Err(e) => {
                error!("Failed to reconnect stream {}: {}", index, e);
                false
            }
        }
    }

    /// Reconnect active stream with exponential backoff
    /// Tries 3 times with 1s, 2s, 4s delays
    /// Returns true if reconnection was successful
    pub fn reconnect_active_with_backoff(&mut self, channel: &crate::channels::Channel) -> bool {
        let Some(idx) = self.active_index else {
            return false;
        };

        let delays = [1, 2, 4]; // seconds
        for (attempt, delay_secs) in delays.iter().enumerate() {
            info!("Reconnect attempt {} of {} (waiting {}s)", attempt + 1, delays.len(), delay_secs);

            std::thread::sleep(Duration::from_secs(*delay_secs));

            if self.reconnect_stream(idx, channel) {
                return true;
            }
        }

        false
    }
}

/// Connect a single stream and return a StreamHandle
/// The stream starts muted and decodes continuously in a background thread
fn connect_single_stream(
    index: usize,
    url: &str,
    stream_handle: OutputStreamHandle,
) -> Result<StreamHandle> {
    // Make HTTP request
    let response = ureq::get(url)
        .set("User-Agent", "lego-radio/1.0")
        .set("Icy-MetaData", "0")
        .call()
        .map_err(|e| anyhow!("HTTP request failed for stream {}: {}", index, e))?;

    let content_type = response.content_type().to_string();
    debug!("Stream {} Content-Type: {}", index, content_type);

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
        .map_err(|e| anyhow!("Failed to probe format for stream {}: {}", index, e))?;

    let mut format = probed.format;

    // Get the default track
    let track = format
        .default_track()
        .ok_or_else(|| anyhow!("No audio track found in stream {}", index))?;

    let track_id = track.id;

    // Create decoder
    let dec_opts = DecoderOptions::default();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &dec_opts)
        .map_err(|e| anyhow!("Failed to create decoder for stream {}: {}", index, e))?;

    // Get audio parameters
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    info!("Stream {}: {} Hz, {} channels", index, sample_rate, channels);

    // Create a sink for playback (starts muted)
    let sink = Arc::new(
        Sink::try_new(&stream_handle)
            .map_err(|e| anyhow!("Failed to create sink for stream {}: {}", index, e))?
    );
    sink.set_volume(MUTED_VOLUME); // Start muted

    // Create shared state for the stream
    let stop_flag = Arc::new(AtomicBool::new(false));
    let status = Arc::new(AtomicU8::new(STREAM_STATUS_CONNECTING));
    let sink_shared: Arc<Mutex<Option<Arc<Sink>>>> = Arc::new(Mutex::new(Some(sink.clone())));

    // Clone for the thread
    let stop_flag_clone = stop_flag.clone();
    let status_clone = status.clone();
    let sink_for_thread = sink;

    // Spawn decode thread
    let thread = thread::spawn(move || {
        // Mark as playing
        status_clone.store(STREAM_STATUS_PLAYING, Ordering::SeqCst);

        // Decode loop
        loop {
            // Check stop flag
            if stop_flag_clone.load(Ordering::SeqCst) {
                debug!("Stream {} stopped by user", index);
                sink_for_thread.stop();
                break;
            }

            // Read next packet
            let packet = match format.next_packet() {
                Ok(packet) => packet,
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    warn!("Stream {} ended", index);
                    status_clone.store(STREAM_STATUS_ERROR, Ordering::SeqCst);
                    break;
                }
                Err(e) => {
                    error!("Stream {} error reading packet: {}", index, e);
                    status_clone.store(STREAM_STATUS_ERROR, Ordering::SeqCst);
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
                    warn!("Stream {} decode error: {}", index, e);
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
            sink_for_thread.append(source);
        }
    });

    Ok(StreamHandle {
        thread,
        stop_flag,
        sink: sink_shared,
        status,
    })
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
    // If ducked, refresh timer so stream gets full DUCK_DURATION_MS from audio start
    let mut is_ducked = should_duck();
    if is_ducked {
        let now_ms = boot_time.elapsed().as_millis() as u64;
        let refreshed_until = now_ms + DUCK_DURATION_MS;
        duck_until_ms.store(refreshed_until, Ordering::SeqCst);
        sink.set_volume(DUCKED_VOLUME);
        debug!(
            "Stream starting ducked (refreshed timer to {}ms)",
            refreshed_until
        );
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

    #[test]
    fn test_muted_volume_is_zero() {
        assert_eq!(MUTED_VOLUME, 0.0);
    }

    #[test]
    fn test_stream_status_constants() {
        assert_eq!(STREAM_STATUS_CONNECTING, 0);
        assert_eq!(STREAM_STATUS_PLAYING, 1);
        assert_eq!(STREAM_STATUS_ERROR, 2);
    }

    #[test]
    fn test_ducked_volume_less_than_normal() {
        assert!(DUCKED_VOLUME < VOLUME);
        assert!(MUTED_VOLUME < DUCKED_VOLUME);
    }
}
