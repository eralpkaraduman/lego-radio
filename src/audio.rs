use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, Sink};
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Playback volume (0.0 to 1.0)
const VOLUME: f32 = 1.0;

/// Beep volume relative to main volume
const BEEP_VOLUME: f32 = 0.3;

// =============================================================================
// AudioPipeline - Simple connect-on-demand audio streaming
// =============================================================================

/// Simple audio pipeline that connects to one stream at a time.
///
/// Key properties:
/// - Connect on demand (no pre-buffering)
/// - One stream at a time
/// - Interruptible (stop clears everything immediately)
/// - TTS plays through same sink
pub struct AudioPipeline {
    /// Single audio output sink
    sink: Arc<Sink>,

    /// Current stream thread (download + decode combined)
    stream_thread: Option<StreamHandle>,

    /// Keep OutputStream alive (audio stops if dropped)
    _stream: OutputStream,
}

/// Handle for controlling the streaming thread
struct StreamHandle {
    thread: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
}

impl StreamHandle {
    fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    fn join(self) {
        let _ = self.thread.join();
    }
}

impl AudioPipeline {
    /// Create a new audio pipeline
    pub fn new() -> Result<Self> {
        let (stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| anyhow!("Failed to open audio output: {}", e))?;

        let sink = Arc::new(
            Sink::try_new(&stream_handle)
                .map_err(|e| anyhow!("Failed to create sink: {}", e))?
        );
        sink.set_volume(VOLUME);

        Ok(Self {
            sink,
            stream_thread: None,
            _stream: stream,
        })
    }

    /// Stop all playback immediately
    ///
    /// Signals stream thread to stop, clears sink. Does NOT wait for thread.
    pub fn stop(&mut self) {
        // Signal stream thread to stop (don't wait - HTTP might be blocking)
        if let Some(handle) = self.stream_thread.take() {
            handle.stop();
            // Don't join - let thread die on its own
            // Spawn a cleanup thread to join it later
            std::thread::spawn(move || {
                handle.join();
            });
        }

        // Clear any queued audio immediately
        self.sink.clear();
        self.sink.play();  // Reset paused state if any
    }

    /// Connect to a URL and start playing
    ///
    /// This is blocking - it waits for initial data before returning.
    /// Returns true if connected successfully, false on error.
    pub fn connect_and_play(&mut self, url: &str) -> bool {
        info!("Connecting to: {}", url);

        // Stop any existing stream
        self.stop();

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let sink = self.sink.clone();
        let url = url.to_string();

        // Channel to signal when connected
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();

        let thread = thread::spawn(move || {
            stream_loop(&url, sink, stop_clone, connected_tx);
        });

        self.stream_thread = Some(StreamHandle { thread, stop_flag });

        // Wait for connection (with timeout)
        match connected_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(true) => {
                info!("Connected and playing");
                true
            }
            Ok(false) => {
                warn!("Connection failed");
                self.stop();
                false
            }
            Err(_) => {
                warn!("Connection timed out");
                self.stop();
                false
            }
        }
    }

    /// Play a short confirmation beep (blocking - waits for beep to finish)
    pub fn beep(&self) {
        // Generate a 80ms 880Hz sine wave (A5 note)
        let sample_rate = 44100u32;
        let duration_ms = 80;
        let frequency = 880.0f32;
        let num_samples = (sample_rate as usize * duration_ms) / 1000;

        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let envelope = if i < num_samples / 10 {
                    // Fade in
                    i as f32 / (num_samples / 10) as f32
                } else if i > num_samples * 9 / 10 {
                    // Fade out
                    (num_samples - i) as f32 / (num_samples / 10) as f32
                } else {
                    1.0
                };
                (t * frequency * 2.0 * std::f32::consts::PI).sin() * BEEP_VOLUME * envelope
            })
            .collect();

        let source = SamplesBuffer::new(1, sample_rate, samples);
        self.sink.append(source);
        self.sink.play();  // Ensure not paused
        self.sink.sleep_until_end();  // Wait for beep to play
    }

    /// Play TTS announcement
    ///
    /// Plays TTS through the sink. Returns true if completed, false if interrupted.
    /// Pass a receiver to check for interrupts, or None for blocking playback.
    pub fn announce_interruptible(
        &mut self,
        text: &str,
        tts: &crate::tts::PiperTts,
        interrupt_rx: Option<&std::sync::mpsc::Receiver<()>>,
    ) -> bool {
        info!("TTS: {}", text);

        // Synthesize
        let samples = match tts.synthesize(text) {
            Ok(s) => s,
            Err(e) => {
                warn!("TTS failed: {}", e);
                return true;
            }
        };

        if samples.is_empty() {
            return true;
        }

        let samples_f32: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();
        let source = SamplesBuffer::new(1, 22050, samples_f32);

        self.sink.append(source);
        self.sink.play();

        // Wait for playback, checking for interrupts
        while !self.sink.empty() {
            if let Some(rx) = interrupt_rx {
                if rx.try_recv().is_ok() {
                    info!("TTS interrupted");
                    self.sink.clear();
                    return false;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        true
    }

    /// Play TTS announcement (blocking, no interrupt check)
    pub fn announce(&mut self, text: &str, tts: &crate::tts::PiperTts) {
        self.announce_interruptible(text, tts, None);
    }
}

// =============================================================================
// Stream Loop - Combined HTTP download and decode
// =============================================================================

/// Combined download and decode loop
///
/// Connects to URL, downloads audio, decodes, and plays to sink.
/// Sends true on connected_tx when first data received, false on error.
fn stream_loop(
    url: &str,
    sink: Arc<Sink>,
    stop_flag: Arc<AtomicBool>,
    connected_tx: std::sync::mpsc::Sender<bool>,
) {
    debug!("Starting stream from: {}", url);

    // Check if HLS stream
    let is_hls = url.contains(".m3u8");

    let mss = if is_hls {
        // HLS stream - use segment reader
        debug!("Detected HLS stream");
        let hls_reader = match HlsReader::new(url, stop_flag.clone()) {
            Ok(r) => r,
            Err(e) => {
                error!("HLS connection failed: {}", e);
                let _ = connected_tx.send(false);
                return;
            }
        };
        let _ = connected_tx.send(true);
        let reader = StoppableReader::new(hls_reader, stop_flag.clone());
        MediaSourceStream::new(Box::new(reader), Default::default())
    } else {
        // Regular stream - direct HTTP
        let response = match ureq::get(url)
            .set("User-Agent", "lego-radio/1.0")
            .set("Icy-MetaData", "0")
            .call()
        {
            Ok(resp) => resp,
            Err(e) => {
                error!("HTTP connection failed: {}", e);
                let _ = connected_tx.send(false);
                return;
            }
        };

        let content_type = response.content_type().to_string();
        debug!("Connected, Content-Type: {}", content_type);
        let _ = connected_tx.send(true);

        let reader = StoppableReader::new(response.into_reader(), stop_flag.clone());
        MediaSourceStream::new(Box::new(reader), Default::default())
    };

    // Probe the format
    let format_opts = FormatOptions {
        enable_gapless: true,
        ..Default::default()
    };
    let metadata_opts = MetadataOptions::default();
    let hint = Hint::new();

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
    let mut packet_count = 0u64;
    loop {
        // Check stop flag
        if stop_flag.load(Ordering::SeqCst) {
            debug!("Stream stopped by user");
            break;
        }

        // Read next packet
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::Interrupted =>
            {
                // Stop flag triggered via StoppableReader
                debug!("Stream interrupted");
                break;
            }
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

        // Ensure playback is active on first packet
        if packet_count == 0 {
            sink.play();
        }
        packet_count += 1;
    }

    debug!("Stream loop ended");
}

// =============================================================================
// HlsReader - Reads HLS streams by fetching segments sequentially
// =============================================================================

/// Reader that fetches HLS segments and presents them as a continuous stream
struct HlsReader {
    /// Base URL for segments (playlist URL without filename)
    base_url: String,
    /// Full playlist URL (for refreshing)
    playlist_url: String,
    /// Current segment data being read
    current_segment: Cursor<Vec<u8>>,
    /// Queue of segment URLs to fetch
    segment_queue: Vec<String>,
    /// Index of next segment in queue
    next_segment_idx: usize,
    /// Last media sequence number seen (for live stream updates)
    last_media_sequence: u64,
    /// Stop flag to check
    stop_flag: Arc<AtomicBool>,
}

impl HlsReader {
    fn new(url: &str, stop_flag: Arc<AtomicBool>) -> Result<Self, std::io::Error> {
        // Extract base URL (everything up to last /)
        let base_url = url.rsplit_once('/')
            .map(|(base, _)| format!("{}/", base))
            .unwrap_or_else(|| url.to_string());

        let mut reader = Self {
            base_url,
            playlist_url: url.to_string(),
            current_segment: Cursor::new(Vec::new()),
            segment_queue: Vec::new(),
            next_segment_idx: 0,
            last_media_sequence: 0,
            stop_flag,
        };

        // Fetch initial playlist
        reader.refresh_playlist()?;

        // Pre-fetch first segment
        reader.fetch_next_segment()?;

        Ok(reader)
    }

    /// Refresh playlist and add new segments to queue
    fn refresh_playlist(&mut self) -> Result<(), std::io::Error> {
        debug!("Fetching HLS playlist: {}", self.playlist_url);

        let response = ureq::get(&self.playlist_url)
            .set("User-Agent", "lego-radio/1.0")
            .call()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut body = String::new();
        response.into_reader().read_to_string(&mut body)?;

        // Parse playlist using m3u8-rs
        let parsed = m3u8_rs::parse_playlist_res(body.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e)))?;

        match parsed {
            m3u8_rs::Playlist::MediaPlaylist(playlist) => {
                let media_sequence = playlist.media_sequence;

                // Only add segments we haven't seen
                if media_sequence > self.last_media_sequence || self.segment_queue.is_empty() {
                    self.segment_queue.clear();
                    self.next_segment_idx = 0;

                    for segment in &playlist.segments {
                        let segment_url = if segment.uri.starts_with("http") {
                            segment.uri.clone()
                        } else {
                            format!("{}{}", self.base_url, segment.uri)
                        };
                        self.segment_queue.push(segment_url);
                    }

                    self.last_media_sequence = media_sequence;
                    debug!("HLS: loaded {} segments", self.segment_queue.len());
                }
            }
            m3u8_rs::Playlist::MasterPlaylist(master) => {
                // Master playlist - pick first variant
                if let Some(variant) = master.variants.first() {
                    let variant_url = if variant.uri.starts_with("http") {
                        variant.uri.clone()
                    } else {
                        format!("{}{}", self.base_url, variant.uri)
                    };
                    debug!("HLS: following master playlist to {}", variant_url);
                    self.playlist_url = variant_url.clone();
                    self.base_url = variant_url.rsplit_once('/')
                        .map(|(base, _)| format!("{}/", base))
                        .unwrap_or(self.base_url.clone());
                    return self.refresh_playlist();
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Empty master playlist",
                ));
            }
        }

        Ok(())
    }

    /// Fetch the next segment into current_segment buffer
    fn fetch_next_segment(&mut self) -> Result<(), std::io::Error> {
        // Check if we need to refresh playlist (live stream)
        if self.next_segment_idx >= self.segment_queue.len() {
            // Give the server a moment before refreshing
            std::thread::sleep(Duration::from_millis(500));
            self.refresh_playlist()?;

            // If still no segments, we're done
            if self.next_segment_idx >= self.segment_queue.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "No more segments",
                ));
            }
        }

        let segment_url = &self.segment_queue[self.next_segment_idx];
        debug!("HLS: fetching segment {}", self.next_segment_idx);

        let response = ureq::get(segment_url)
            .set("User-Agent", "lego-radio/1.0")
            .call()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        let mut data = Vec::new();
        response.into_reader().read_to_end(&mut data)?;

        self.current_segment = Cursor::new(data);
        self.next_segment_idx += 1;

        Ok(())
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Check stop flag
        if self.stop_flag.load(Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Stopped",
            ));
        }

        // Try to read from current segment
        let n = self.current_segment.read(buf)?;

        if n > 0 {
            return Ok(n);
        }

        // Current segment exhausted, fetch next one
        match self.fetch_next_segment() {
            Ok(()) => self.current_segment.read(buf),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(e) => Err(e),
        }
    }
}

// =============================================================================
// StoppableReader - Wrapper that checks stop flag on reads
// =============================================================================

/// Reader wrapper that returns Interrupted error when stop flag is set
struct StoppableReader<R> {
    inner: R,
    stop_flag: Arc<AtomicBool>,
}

impl<R> StoppableReader<R> {
    fn new(inner: R, stop_flag: Arc<AtomicBool>) -> Self {
        Self { inner, stop_flag }
    }

    fn is_stopped(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }
}

impl<R: Read> Read for StoppableReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.is_stopped() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Stopped",
            ));
        }
        self.inner.read(buf)
    }
}

impl<R: Read> std::io::Seek for StoppableReader<R> {
    fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Seeking not supported for streams",
        ))
    }
}

impl<R: Read + Send + Sync> symphonia::core::io::MediaSource for StoppableReader<R> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_constant_in_range() {
        assert!(VOLUME >= 0.0);
        assert!(VOLUME <= 1.0);
    }

    #[test]
    fn test_stop_flag_works() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        assert!(!stop_flag.load(Ordering::SeqCst));

        stop_flag.store(true, Ordering::SeqCst);
        assert!(stop_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn test_audio_pipeline_creation() {
        // Skip if no audio device available
        let _pipeline = match AudioPipeline::new() {
            Ok(p) => p,
            Err(_) => return,
        };
    }
}
