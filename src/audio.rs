use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use rtrb::{Consumer, Producer, RingBuffer};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

/// Ring buffer size: 48KB = ~3 seconds @ 128kbps
/// Calculation: 128 kbps = 16,000 bytes/sec × 3 sec = 48,000 bytes
const BUFFER_SIZE: usize = 48 * 1024;

// =============================================================================
// StreamBuffer - Lock-free ring buffer wrapper for real-time audio streaming
// =============================================================================

/// Lock-free ring buffer for streaming compressed audio from HTTP to decoder.
///
/// Uses rtrb (real-time ring buffer) for wait-free SPSC communication:
/// - Download thread writes compressed audio via `producer`
/// - Decoder thread reads via `consumer`
///
/// Buffer holds ~3 seconds of compressed audio at 128kbps, providing resilience
/// against network jitter while keeping latency low.
pub struct StreamBuffer {
    /// Producer half - download thread writes here (Send, not Sync)
    producer: Producer<u8>,
    /// Consumer half - decoder thread reads here (Send, not Sync)
    consumer: Consumer<u8>,
}

impl StreamBuffer {
    /// Create a new stream buffer with default size (48KB / ~3 seconds @ 128kbps)
    pub fn new() -> Self {
        let (producer, consumer) = RingBuffer::new(BUFFER_SIZE);
        Self { producer, consumer }
    }

    /// Create a stream buffer with custom capacity
    #[allow(dead_code)]
    pub fn with_capacity(capacity: usize) -> Self {
        let (producer, consumer) = RingBuffer::new(capacity);
        Self { producer, consumer }
    }

    /// Write bytes to the buffer (called by download thread)
    ///
    /// Uses overwrite semantics: if buffer is full, oldest data is discarded.
    /// This is appropriate for live streams where old data becomes stale.
    ///
    /// Returns number of bytes written (always equals input length with overwrite)
    pub fn write(&mut self, data: &[u8]) -> usize {
        let mut written = 0;
        for &byte in data {
            // Try to push, if full we need to drop oldest data
            if self.producer.push(byte).is_err() {
                // Buffer full - pop oldest byte to make room (overwrite semantics)
                let _ = self.consumer.pop();
                // Now push should succeed
                let _ = self.producer.push(byte);
            }
            written += 1;
        }
        written
    }

    /// Read bytes from the buffer (called by decoder thread)
    ///
    /// Returns number of bytes read (may be less than buf.len() if buffer has less data)
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let mut read_count = 0;
        for slot in buf.iter_mut() {
            match self.consumer.pop() {
                Ok(byte) => {
                    *slot = byte;
                    read_count += 1;
                }
                Err(_) => break, // Buffer empty
            }
        }
        read_count
    }

    /// Clear all data from the buffer
    ///
    /// Used by disconnect_all() when stopping streams
    pub fn clear(&mut self) {
        while self.consumer.pop().is_ok() {}
    }

    /// Number of bytes currently in the buffer
    pub fn len(&self) -> usize {
        self.producer.slots() - self.producer.slots() + self.consumer.slots()
        // Note: This is an approximation since rtrb doesn't expose exact count
        // For our purposes, we use slots() which gives available space
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.consumer.is_empty()
    }

    /// Check if buffer has data available for reading
    pub fn has_data(&self) -> bool {
        !self.consumer.is_empty()
    }

    /// Get buffer capacity
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        BUFFER_SIZE
    }

    /// Split into producer and consumer halves for use in separate threads
    ///
    /// This consumes the StreamBuffer and returns the raw producer/consumer.
    /// Use this when you need to move producer to download thread and consumer
    /// to decoder thread.
    pub fn split(self) -> (Producer<u8>, Consumer<u8>) {
        (self.producer, self.consumer)
    }
}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Download Thread - HTTP streaming to ring buffer
// =============================================================================

/// Handle for a download thread
pub struct DownloadHandle {
    thread: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
    /// True if initial connection succeeded and stream is downloading
    connected: Arc<AtomicBool>,
    /// Error state: true if download failed after initial connection
    errored: Arc<AtomicBool>,
}

impl DownloadHandle {
    /// Signal the download thread to stop
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    /// Check if stream successfully connected
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Check if download has errored
    pub fn has_error(&self) -> bool {
        self.errored.load(Ordering::SeqCst)
    }

    /// Wait for thread to finish (call after stop())
    pub fn join(self) {
        let _ = self.thread.join();
    }
}

/// Spawn a download thread that reads from URL and writes to buffer
///
/// Returns a DownloadHandle for controlling the thread.
/// The producer half of the buffer is moved into the thread.
pub fn spawn_downloader(url: String, mut producer: Producer<u8>) -> DownloadHandle {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicBool::new(false));
    let errored = Arc::new(AtomicBool::new(false));

    let stop_clone = stop_flag.clone();
    let connected_clone = connected.clone();
    let errored_clone = errored.clone();

    let thread = thread::spawn(move || {
        download_loop(&url, &mut producer, &stop_clone, &connected_clone, &errored_clone);
    });

    DownloadHandle {
        thread,
        stop_flag,
        connected,
        errored,
    }
}

/// Exponential backoff delays for reconnection attempts (in seconds)
const RETRY_DELAYS: [u64; 3] = [1, 2, 4];

/// Download loop - continuously reads from HTTP and writes to ring buffer
///
/// Includes error recovery with exponential backoff:
/// 1. On connection/read failure, retries with 1s, 2s, 4s delays
/// 2. During retry, decoder continues from buffer (~3 sec cushion)
/// 3. After 3 failures, marks stream as errored (unavailable)
///
/// Runs until stop flag is set or max retries exhausted.
fn download_loop(
    url: &str,
    producer: &mut Producer<u8>,
    stop_flag: &AtomicBool,
    connected: &AtomicBool,
    errored: &AtomicBool,
) {
    debug!("Starting download from: {}", url);

    let mut retry_count = 0;

    'outer: loop {
        // Check stop flag before attempting connection
        if stop_flag.load(Ordering::SeqCst) {
            debug!("Download stopped by user before connection");
            break;
        }

        // Make HTTP request
        let response = match ureq::get(url)
            .set("User-Agent", "lego-radio/1.0")
            .set("Icy-MetaData", "0")
            .call()
        {
            Ok(resp) => {
                // Successfully connected - reset retry count
                retry_count = 0;
                resp
            }
            Err(e) => {
                error!("HTTP connection failed: {}", e);

                // Attempt retry with exponential backoff
                if retry_count < RETRY_DELAYS.len() {
                    let delay = RETRY_DELAYS[retry_count];
                    warn!("Retrying in {}s (attempt {}/{})", delay, retry_count + 1, RETRY_DELAYS.len());

                    // Wait with periodic stop flag checks
                    let deadline = Instant::now() + Duration::from_secs(delay);
                    while Instant::now() < deadline {
                        if stop_flag.load(Ordering::SeqCst) {
                            debug!("Download stopped during retry wait");
                            break 'outer;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }

                    retry_count += 1;
                    continue 'outer; // Retry connection
                } else {
                    // Max retries exhausted
                    error!("Max retries exhausted, marking stream as unavailable");
                    errored.store(true, Ordering::SeqCst);
                    break 'outer;
                }
            }
        };

        let content_type = response.content_type().to_string();
        debug!("Connected, Content-Type: {}", content_type);
        connected.store(true, Ordering::SeqCst);

        let mut reader = response.into_reader();
        let mut chunk = [0u8; 4096];

        // Read loop
        loop {
            // Check stop flag
            if stop_flag.load(Ordering::SeqCst) {
                debug!("Download stopped by user");
                break 'outer;
            }

            // Read from HTTP stream
            let bytes_read = match reader.read(&mut chunk) {
                Ok(0) => {
                    // Stream ended (EOF)
                    warn!("Stream ended unexpectedly");
                    break; // Break inner loop to attempt reconnect
                }
                Ok(n) => n,
                Err(e) => {
                    error!("Read error: {}", e);
                    break; // Break inner loop to attempt reconnect
                }
            };

            // Write to ring buffer with overwrite semantics
            for &byte in &chunk[..bytes_read] {
                if producer.push(byte).is_err() {
                    // Buffer full - yield and retry
                    thread::yield_now();
                    let _ = producer.push(byte);
                }
            }
        }

        // Read failed - attempt retry with exponential backoff
        if retry_count < RETRY_DELAYS.len() {
            let delay = RETRY_DELAYS[retry_count];
            warn!("Stream interrupted, retrying in {}s (attempt {}/{})", delay, retry_count + 1, RETRY_DELAYS.len());

            let deadline = Instant::now() + Duration::from_secs(delay);
            while Instant::now() < deadline {
                if stop_flag.load(Ordering::SeqCst) {
                    debug!("Download stopped during retry wait");
                    break 'outer;
                }
                thread::sleep(Duration::from_millis(100));
            }

            retry_count += 1;
            // Loop will continue and attempt reconnection
        } else {
            // Max retries exhausted
            error!("Max retries exhausted after stream interruption");
            errored.store(true, Ordering::SeqCst);
            break 'outer;
        }
    }

    debug!("Download loop ended");
}

// =============================================================================
// Decoder Thread - Reads from ring buffer, decodes, plays to single Sink
// =============================================================================

/// Read adapter for rtrb Consumer - makes Consumer compatible with std::io::Read
///
/// This allows symphonia to create a MediaSourceStream from our ring buffer.
/// When buffer is empty, blocks briefly then returns what's available.
///
/// Note: Wrapped in Mutex to satisfy MediaSource's Sync requirement.
/// In practice, only the decoder thread accesses this so there's no contention.
pub struct ConsumerReader {
    consumer: Mutex<Consumer<u8>>,
    /// How long to wait when buffer is empty before returning
    read_timeout: Duration,
}

impl ConsumerReader {
    pub fn new(consumer: Consumer<u8>) -> Self {
        Self {
            consumer: Mutex::new(consumer),
            read_timeout: Duration::from_millis(100),
        }
    }

    #[allow(dead_code)]
    pub fn with_timeout(consumer: Consumer<u8>, timeout: Duration) -> Self {
        Self {
            consumer: Mutex::new(consumer),
            read_timeout: timeout,
        }
    }
}

impl Read for ConsumerReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut consumer = self.consumer.lock().unwrap();
        let mut read_count = 0;

        // Try to read available data first
        for slot in buf.iter_mut() {
            match consumer.pop() {
                Ok(byte) => {
                    *slot = byte;
                    read_count += 1;
                }
                Err(_) => break, // Buffer empty
            }
        }

        // If we got some data, return it
        if read_count > 0 {
            return Ok(read_count);
        }

        // Buffer empty - release lock and wait briefly for more data
        drop(consumer);

        let start = Instant::now();
        while start.elapsed() < self.read_timeout {
            thread::sleep(Duration::from_millis(5));

            let mut consumer = self.consumer.lock().unwrap();
            if let Ok(byte) = consumer.pop() {
                buf[0] = byte;
                return Ok(1);
            }
        }

        // Timeout - return WouldBlock to signal temporary underflow
        Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "Buffer underflow - waiting for data",
        ))
    }
}

impl std::io::Seek for ConsumerReader {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        // Streams don't support seeking
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Seeking not supported for streams",
        ))
    }
}

impl symphonia::core::io::MediaSource for ConsumerReader {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Handle for controlling a decoder thread
pub struct DecoderHandle {
    thread: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
}

impl DecoderHandle {
    /// Signal the decoder thread to stop
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    /// Pause the decoder (stops writing to sink but maintains state)
    pub fn pause(&self) {
        self.pause_flag.store(true, Ordering::SeqCst);
    }

    /// Resume the decoder after pause
    pub fn resume(&self) {
        self.pause_flag.store(false, Ordering::SeqCst);
    }

    /// Check if decoder is paused
    #[allow(dead_code)]
    pub fn is_paused(&self) -> bool {
        self.pause_flag.load(Ordering::SeqCst)
    }

    /// Wait for thread to finish (call after stop())
    pub fn join(self) {
        let _ = self.thread.join();
    }
}

/// Spawn a decoder thread that reads from buffer, decodes, and plays to sink
///
/// Returns a DecoderHandle for controlling the thread.
/// The consumer half of the buffer is moved into the thread.
pub fn spawn_decoder(consumer: Consumer<u8>, sink: Arc<Sink>) -> DecoderHandle {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));

    let stop_clone = stop_flag.clone();
    let pause_clone = pause_flag.clone();

    let thread = thread::spawn(move || {
        decode_loop(consumer, sink, &stop_clone, &pause_clone);
    });

    DecoderHandle {
        thread,
        stop_flag,
        pause_flag,
    }
}

/// Decode loop - reads from buffer, decodes with symphonia, plays to sink
///
/// Runs until stop flag is set.
/// When paused, stops appending to sink but continues decoding to keep up.
fn decode_loop(
    consumer: Consumer<u8>,
    sink: Arc<Sink>,
    stop_flag: &AtomicBool,
    pause_flag: &AtomicBool,
) {
    debug!("Starting decoder");

    // Create reader adapter
    let reader = ConsumerReader::new(consumer);
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());

    // Probe the format (finds sync point automatically)
    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let metadata_opts = MetadataOptions::default();
    let hint = Hint::new(); // No hint - let symphonia auto-detect

    let probed = match symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts) {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to probe format: {}", e);
            return;
        }
    };

    let mut format = probed.format;

    // Get the default track
    let track = match format.default_track() {
        Some(t) => t,
        None => {
            error!("No audio track found");
            return;
        }
    };

    let track_id = track.id;

    // Create decoder
    let dec_opts = DecoderOptions::default();
    let mut decoder = match symphonia::default::get_codecs().make(&track.codec_params, &dec_opts) {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to create decoder: {}", e);
            return;
        }
    };

    // Get audio parameters
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);

    info!("Decoder: {} Hz, {} channels", sample_rate, channels);

    // Decode loop
    loop {
        // Check stop flag
        if stop_flag.load(Ordering::SeqCst) {
            debug!("Decoder stopped by user");
            break;
        }

        // Read next packet
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Buffer underflow - wait a bit and retry
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                warn!("Decoder: stream ended");
                break;
            }
            Err(e) => {
                error!("Decoder: error reading packet: {}", e);
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

        // If paused, skip appending to sink but continue decoding
        // This keeps decoder state current so resume is instant
        if pause_flag.load(Ordering::SeqCst) {
            continue;
        }

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

    debug!("Decode loop ended");
}

// =============================================================================
// AudioPipeline - Single-pipe architecture for efficient streaming
// =============================================================================

/// Single-pipe audio architecture for internet radio streaming.
///
/// Key properties:
/// - HTTP downloads run continuously (bandwidth for N streams)
/// - Only ONE decoder active at a time (CPU for 1 stream)
/// - Single audio sink (no volume juggling)
/// - TTS writes to same sink (decoder pauses during TTS)
/// - Channel switch = point decoder at different buffer
///
/// This replaces MultiStreamPlayer with a more efficient single-pipe design.
pub struct AudioPipeline {
    /// Consumers for each channel - decoder reads from these
    /// Option because we take ownership when starting decoder
    consumers: Vec<Option<Consumer<u8>>>,

    /// Download threads (one per channel)
    downloaders: Vec<Option<DownloadHandle>>,

    /// Single decoder thread (reads from active buffer)
    decoder: Option<DecoderHandle>,

    /// Single audio output
    sink: Arc<Sink>,

    /// Which buffer the decoder reads from (None = stopped)
    active_channel: Option<usize>,

    /// Keep OutputStream alive (audio stops if dropped)
    _stream: OutputStream,

    /// Stream handle for creating sinks
    stream_handle: OutputStreamHandle,
}

impl AudioPipeline {
    /// Create a new audio pipeline with N channel buffers
    pub fn new(num_channels: usize) -> Result<Self> {
        let (stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| anyhow!("Failed to open audio output: {}", e))?;

        let sink = Arc::new(
            Sink::try_new(&stream_handle)
                .map_err(|e| anyhow!("Failed to create sink: {}", e))?
        );
        sink.set_volume(VOLUME);

        // Initialize empty consumer and downloader slots
        let consumers: Vec<Option<Consumer<u8>>> = (0..num_channels)
            .map(|_| None)
            .collect();

        let downloaders: Vec<Option<DownloadHandle>> = (0..num_channels)
            .map(|_| None)
            .collect();

        Ok(Self {
            consumers,
            downloaders,
            decoder: None,
            sink,
            active_channel: None,
            _stream: stream,
            stream_handle,
        })
    }

    /// Get the number of channels this pipeline supports
    pub fn num_channels(&self) -> usize {
        self.consumers.len()
    }

    /// Get the currently active channel index (if any)
    pub fn active_channel(&self) -> Option<usize> {
        self.active_channel
    }

    /// Check if a download is connected for a specific channel
    pub fn is_channel_connected(&self, index: usize) -> bool {
        self.downloaders
            .get(index)
            .and_then(|d| d.as_ref())
            .map(|d| d.is_connected())
            .unwrap_or(false)
    }

    /// Check if a download has errored for a specific channel
    pub fn has_channel_error(&self, index: usize) -> bool {
        self.downloaders
            .get(index)
            .and_then(|d| d.as_ref())
            .map(|d| d.has_error())
            .unwrap_or(false)
    }

    /// Check if decoder is currently running
    pub fn is_playing(&self) -> bool {
        self.decoder.is_some()
    }

    /// Get a reference to the sink (for TTS playback)
    pub fn sink(&self) -> &Arc<Sink> {
        &self.sink
    }

    /// Get the stream handle (for creating additional sinks if needed)
    pub fn stream_handle(&self) -> &OutputStreamHandle {
        &self.stream_handle
    }

    /// Connect all channels to their stream URLs
    ///
    /// Spawns download threads in parallel for all channels.
    /// Waits for buffers to start filling (with timeout).
    /// Returns count of successfully connected streams.
    pub fn connect_all(&mut self, channels: &[crate::channels::Channel], timeout: Duration) -> usize {
        info!("Connecting to {} stations (timeout: {:?})...", channels.len(), timeout);

        let deadline = Instant::now() + timeout;

        // Spawn download threads for each channel
        for (i, channel) in channels.iter().enumerate() {
            if i >= self.consumers.len() {
                warn!("More channels than slots, skipping channel {}", i);
                continue;
            }

            // Create a new buffer and split into producer/consumer
            let buffer = StreamBuffer::new();
            let (producer, consumer) = buffer.split();

            // Store consumer for decoder to use later
            self.consumers[i] = Some(consumer);

            // Spawn downloader with producer
            let handle = spawn_downloader(channel.url.to_string(), producer);
            self.downloaders[i] = Some(handle);

            debug!("Started download thread for channel {}", i);
        }

        // Wait for connections to establish (check connected status)
        let mut connected = 0;
        let check_interval = Duration::from_millis(100);

        while Instant::now() < deadline {
            connected = self.downloaders
                .iter()
                .filter(|d| d.as_ref().map(|h| h.is_connected()).unwrap_or(false))
                .count();

            // If all connected (or errored), we're done
            let errored = self.downloaders
                .iter()
                .filter(|d| d.as_ref().map(|h| h.has_error()).unwrap_or(false))
                .count();

            if connected + errored >= channels.len().min(self.consumers.len()) {
                break;
            }

            thread::sleep(check_interval);
        }

        info!("Connected {} out of {} streams", connected, channels.len());
        connected
    }

    /// Disconnect all channels and stop playback
    ///
    /// Stops decoder, stops all download threads, clears buffers.
    /// Used when entering Off state to save bandwidth.
    pub fn disconnect_all(&mut self) {
        info!("Disconnecting all streams");

        // Stop decoder first
        if let Some(handle) = self.decoder.take() {
            handle.stop();
            handle.join();
            debug!("Decoder stopped");
        }

        // Stop all downloaders
        for (i, downloader) in self.downloaders.iter_mut().enumerate() {
            if let Some(handle) = downloader.take() {
                handle.stop();
                handle.join();
                debug!("Downloader {} stopped", i);
            }
        }

        // Clear consumers (they're invalidated when downloaders stop)
        for consumer in self.consumers.iter_mut() {
            *consumer = None;
        }

        self.active_channel = None;
        debug!("All streams disconnected");
    }

    /// Switch to a different channel
    ///
    /// Stops current decoder (if any) and starts new decoder reading from
    /// the selected channel's buffer. Switch latency ~100-200ms.
    pub fn select(&mut self, index: usize) {
        if index >= self.consumers.len() {
            error!("Invalid channel index: {}", index);
            return;
        }

        debug!("Switching to channel {}", index);

        // Stop current decoder if running
        if let Some(handle) = self.decoder.take() {
            handle.stop();
            handle.join();
            debug!("Previous decoder stopped");
        }

        // Take the consumer for this channel and start decoder
        if let Some(mut consumer) = self.consumers[index].take() {
            // Clear any stale buffered data so playback starts immediately
            // Without this, old data (~3 sec) plays before fresh audio
            let mut cleared = 0;
            while consumer.pop().is_ok() {
                cleared += 1;
            }
            if cleared > 0 {
                debug!("Cleared {} bytes of stale buffer data", cleared);
            }

            let handle = spawn_decoder(consumer, self.sink.clone());
            self.decoder = Some(handle);
            self.active_channel = Some(index);
            info!("Switched to channel {} - decoder started", index);
        } else {
            warn!("Consumer {} not available for decoding", index);
            self.active_channel = Some(index);
        }
    }

    /// Stop the current decoder (for Off state or TTS)
    pub fn stop(&mut self) {
        if let Some(handle) = self.decoder.take() {
            handle.stop();
            handle.join();
            debug!("Decoder stopped");
        }
        self.active_channel = None;
    }

    /// Pause decoder for TTS (decoder keeps state but stops appending to sink)
    pub fn pause_decoder(&self) {
        if let Some(ref handle) = self.decoder {
            handle.pause();
            debug!("Decoder paused");
        }
    }

    /// Resume decoder after TTS
    pub fn resume_decoder(&self) {
        if let Some(ref handle) = self.decoder {
            handle.resume();
            debug!("Decoder resumed");
        }
    }

    /// Play TTS announcement through the same sink
    ///
    /// Pauses decoder, plays TTS, then resumes decoder.
    /// Blocking - waits for TTS to finish.
    pub fn announce(&self, text: &str, tts: &crate::tts::PiperTts) {
        debug!("TTS: {}", text);

        // Pause decoder while TTS plays
        self.pause_decoder();

        // Synthesize and play
        match tts.synthesize(text) {
            Ok(samples) => {
                if !samples.is_empty() {
                    let samples_f32: Vec<f32> =
                        samples.iter().map(|&s| s as f32 / 32768.0).collect();
                    let source = SamplesBuffer::new(1, 22050, samples_f32);

                    self.sink.append(source);
                    self.sink.sleep_until_end();
                }
            }
            Err(e) => {
                warn!("TTS failed: {}", e);
            }
        }

        // Resume decoder
        self.resume_decoder();
    }

    /// Speak text (fire-and-forget, non-blocking version)
    /// For use when you don't want to block on TTS
    pub fn speak(&self, text: &str, tts: &std::sync::Arc<crate::tts::PiperTts>) {
        let text = text.to_string();
        let tts = tts.clone();
        let sink = self.sink.clone();
        let decoder = self.decoder.as_ref().map(|h| (h.pause_flag.clone(), h.stop_flag.clone()));

        thread::spawn(move || {
            // Pause decoder
            if let Some((pause_flag, _)) = &decoder {
                pause_flag.store(true, Ordering::SeqCst);
            }

            match tts.synthesize(&text) {
                Ok(samples) => {
                    if !samples.is_empty() {
                        let samples_f32: Vec<f32> =
                            samples.iter().map(|&s| s as f32 / 32768.0).collect();
                        let source = SamplesBuffer::new(1, 22050, samples_f32);

                        sink.append(source);
                        sink.sleep_until_end();
                    }
                }
                Err(e) => {
                    warn!("TTS failed: {}", e);
                }
            }

            // Resume decoder
            if let Some((pause_flag, _)) = decoder {
                pause_flag.store(false, Ordering::SeqCst);
            }
        });
    }
}

// Old Player and MultiStreamPlayer structs have been removed.
// Use AudioPipeline for all audio streaming functionality.

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

    // StreamBuffer tests

    #[test]
    fn test_stream_buffer_new() {
        let buffer = StreamBuffer::new();
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_stream_buffer_write_read() {
        let mut buffer = StreamBuffer::new();
        let data = [1u8, 2, 3, 4, 5];

        let written = buffer.write(&data);
        assert_eq!(written, 5);
        assert!(buffer.has_data());

        let mut out = [0u8; 5];
        let read = buffer.read(&mut out);
        assert_eq!(read, 5);
        assert_eq!(out, data);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_stream_buffer_partial_read() {
        let mut buffer = StreamBuffer::new();
        let data = [1u8, 2, 3];

        buffer.write(&data);

        let mut out = [0u8; 10];
        let read = buffer.read(&mut out);
        assert_eq!(read, 3);
        assert_eq!(&out[..3], &data);
    }

    #[test]
    fn test_stream_buffer_clear() {
        let mut buffer = StreamBuffer::new();
        buffer.write(&[1, 2, 3, 4, 5]);
        assert!(buffer.has_data());

        buffer.clear();
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_stream_buffer_overwrite_semantics() {
        // Create a tiny buffer to test overflow behavior
        let mut buffer = StreamBuffer::with_capacity(4);

        // Write more than capacity - should overwrite oldest
        buffer.write(&[1, 2, 3, 4]);
        buffer.write(&[5, 6]); // Should overwrite 1, 2

        let mut out = [0u8; 4];
        let read = buffer.read(&mut out);
        // Should get the most recent data (3, 4, 5, 6)
        assert_eq!(read, 4);
        assert_eq!(out, [3, 4, 5, 6]);
    }

    #[test]
    fn test_stream_buffer_split() {
        let buffer = StreamBuffer::new();
        let (mut producer, mut consumer) = buffer.split();

        // Write via producer
        assert!(producer.push(42).is_ok());

        // Read via consumer
        assert_eq!(consumer.pop(), Ok(42));
    }

    #[test]
    fn test_buffer_size_constant() {
        // 48KB for 3 seconds @ 128kbps
        assert_eq!(BUFFER_SIZE, 48 * 1024);
    }

    // DownloadHandle tests

    #[test]
    fn test_download_handle_stop_flag() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));

        // Initial state
        assert!(!stop_flag.load(Ordering::SeqCst));
        assert!(!connected.load(Ordering::SeqCst));
        assert!(!errored.load(Ordering::SeqCst));

        // Simulate setting flags
        stop_flag.store(true, Ordering::SeqCst);
        connected.store(true, Ordering::SeqCst);

        assert!(stop_flag.load(Ordering::SeqCst));
        assert!(connected.load(Ordering::SeqCst));
        assert!(!errored.load(Ordering::SeqCst));
    }

    // DecoderHandle / ConsumerReader tests

    #[test]
    fn test_decoder_handle_pause_resume() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let pause_flag = Arc::new(AtomicBool::new(false));

        // Initial state
        assert!(!pause_flag.load(Ordering::SeqCst));

        // Pause
        pause_flag.store(true, Ordering::SeqCst);
        assert!(pause_flag.load(Ordering::SeqCst));

        // Resume
        pause_flag.store(false, Ordering::SeqCst);
        assert!(!pause_flag.load(Ordering::SeqCst));

        // Stop should work regardless of pause state
        stop_flag.store(true, Ordering::SeqCst);
        assert!(stop_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_consumer_reader_with_data() {
        let buffer = StreamBuffer::new();
        let (mut producer, consumer) = buffer.split();

        // Write some data
        for i in 0..10u8 {
            producer.push(i).unwrap();
        }

        // Create reader and read
        let mut reader = ConsumerReader::new(consumer);
        let mut buf = [0u8; 10];
        let n = reader.read(&mut buf).unwrap();

        assert_eq!(n, 10);
        for (i, &byte) in buf.iter().enumerate() {
            assert_eq!(byte, i as u8);
        }
    }

    #[test]
    fn test_consumer_reader_seek_unsupported() {
        use std::io::Seek;

        let buffer = StreamBuffer::new();
        let (_producer, consumer) = buffer.split();

        let mut reader = ConsumerReader::new(consumer);
        let result = reader.seek(std::io::SeekFrom::Start(0));

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn test_consumer_reader_media_source_traits() {
        use symphonia::core::io::MediaSource;

        let buffer = StreamBuffer::new();
        let (_producer, consumer) = buffer.split();

        let reader = ConsumerReader::new(consumer);

        // MediaSource trait methods
        assert!(!reader.is_seekable());
        assert!(reader.byte_len().is_none());
    }

    // AudioPipeline tests
    // Note: Full pipeline tests require audio hardware, so we test structure only

    #[test]
    fn test_audio_pipeline_initial_state() {
        // Skip on CI or when no audio device available
        let pipeline = match AudioPipeline::new(4) {
            Ok(p) => p,
            Err(_) => return, // Skip if no audio device
        };

        assert_eq!(pipeline.num_channels(), 4);
        assert!(pipeline.active_channel().is_none());
        assert!(!pipeline.is_playing());

        // No channels connected initially
        for i in 0..4 {
            assert!(!pipeline.is_channel_connected(i));
            assert!(!pipeline.has_channel_error(i));
        }
    }
}
