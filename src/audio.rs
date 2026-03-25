use crate::button::ButtonEvent;
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

/// Beep volume
const BEEP_VOLUME: f32 = 0.3;

/// Ducked stream volume during browsing (used by upcoming browse state machine)
#[allow(dead_code)]
const DUCK_VOLUME: f32 = 0.2;

// =============================================================================
// AudioPipeline - Simple connect-on-demand audio streaming
// =============================================================================

/// Audio pipeline with three independent sinks.
///
/// - **stream_sink**: Radio stream playback, volume duckable
/// - **voice_sink**: TTS announcements, interruptible
/// - **beep_sink**: Beeps, chirps, confirmation tunes — never interrupted
///
/// All three share one OutputStream and can play simultaneously.
pub struct AudioPipeline {
    /// Radio stream playback (duckable)
    stream_sink: Arc<Sink>,
    /// TTS voice announcements (interruptible)
    voice_sink: Sink,
    /// Beeps and confirmation tunes
    beep_sink: Sink,

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

    fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }
}

impl AudioPipeline {
    /// Create a new audio pipeline with three independent sinks
    pub fn new() -> Result<Self> {
        let (stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| anyhow!("Failed to open audio output: {}", e))?;

        let stream_sink = Arc::new(
            Sink::try_new(&stream_handle)
                .map_err(|e| anyhow!("Failed to create stream sink: {}", e))?,
        );
        stream_sink.set_volume(VOLUME);

        let voice_sink = Sink::try_new(&stream_handle)
            .map_err(|e| anyhow!("Failed to create voice sink: {}", e))?;
        voice_sink.set_volume(VOLUME);

        let beep_sink = Sink::try_new(&stream_handle)
            .map_err(|e| anyhow!("Failed to create beep sink: {}", e))?;
        beep_sink.set_volume(BEEP_VOLUME);

        info!("Audio pipeline initialized (3 sinks)");
        sentry::add_breadcrumb(sentry::Breadcrumb {
            category: Some("audio".into()),
            message: Some("Audio pipeline initialized".into()),
            level: sentry::Level::Info,
            ..Default::default()
        });

        Ok(Self {
            stream_sink,
            voice_sink,
            beep_sink,
            stream_thread: None,
            _stream: stream,
        })
    }

    /// Stop stream playback. Does NOT affect voice or beep sinks.
    pub fn stop_stream(&mut self) {
        if let Some(handle) = self.stream_thread.take() {
            handle.stop();
            std::thread::spawn(move || {
                handle.join();
            });
        }
        self.stream_sink.clear();
        self.stream_sink.play();
    }

    /// Stop all playback immediately (stream + voice + beep)
    pub fn stop(&mut self) {
        self.stop_stream();
        self.voice_sink.clear();
        self.voice_sink.play();
        self.beep_sink.clear();
        self.beep_sink.play();
    }

    /// Duck stream volume for browsing
    #[allow(dead_code)]
    pub fn duck_stream(&self) {
        self.stream_sink.set_volume(DUCK_VOLUME);
    }

    /// Restore stream volume after browsing
    #[allow(dead_code)]
    pub fn restore_stream(&self) {
        self.stream_sink.set_volume(VOLUME);
    }

    /// Connect to a URL and start playing
    ///
    /// This is blocking - it waits for initial data before returning.
    /// Returns true if connected successfully, false on error.
    pub fn connect_and_play(&mut self, url: &str) -> bool {
        info!("Connecting to: {}", url);

        // Stop any existing stream
        self.stop_stream();

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let sink = self.stream_sink.clone();
        let url_string = url.to_string();
        let url_for_thread = url_string.clone();

        // Channel to signal when connected
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();

        let thread = thread::spawn(move || {
            stream_loop(&url_for_thread, sink, stop_clone, connected_tx);
        });

        self.stream_thread = Some(StreamHandle { thread, stop_flag });

        // Wait for connection (with timeout)
        match connected_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(true) => {
                info!("Connected and playing");
                sentry::add_breadcrumb(sentry::Breadcrumb {
                    category: Some("stream".into()),
                    message: Some(format!("Connected to stream: {}", url_string)),
                    level: sentry::Level::Info,
                    ..Default::default()
                });
                true
            }
            Ok(false) => {
                warn!("Connection failed");
                sentry::add_breadcrumb(sentry::Breadcrumb {
                    category: Some("stream".into()),
                    message: Some(format!("Connection failed: {}", url_string)),
                    level: sentry::Level::Warning,
                    ..Default::default()
                });
                self.stop();
                false
            }
            Err(_) => {
                warn!("Connection timed out");
                sentry::add_breadcrumb(sentry::Breadcrumb {
                    category: Some("stream".into()),
                    message: Some(format!("Connection timeout: {}", url_string)),
                    level: sentry::Level::Warning,
                    ..Default::default()
                });
                self.stop();
                false
            }
        }
    }

    /// Check if stream is still active (not disconnected)
    pub fn is_stream_active(&self) -> bool {
        match &self.stream_thread {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }

    /// Start continuous beep (non-blocking) - plays until stop_beep is called
    pub fn start_beep(&self) {
        let sample_rate = 44100u32;
        let duration_ms = 5000;
        let frequency = 880.0f32;
        let num_samples = (sample_rate as usize * duration_ms) / 1000;
        let fade_samples = sample_rate as usize / 20;

        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let envelope = if i < fade_samples {
                    i as f32 / fade_samples as f32
                } else {
                    1.0
                };
                (t * frequency * 2.0 * std::f32::consts::PI).sin() * envelope
            })
            .collect();

        let source = SamplesBuffer::new(1, sample_rate, samples);
        self.beep_sink.append(source);
        self.beep_sink.play();
    }

    /// Stop the continuous beep with a quick fade-out
    pub fn stop_beep(&self) {
        let sample_rate = 44100u32;
        let fade_ms = 15;
        let frequency = 880.0f32;
        let num_samples = (sample_rate as usize * fade_ms) / 1000;

        let samples: Vec<f32> = (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let envelope = 1.0 - (i as f32 / num_samples as f32);
                (t * frequency * 2.0 * std::f32::consts::PI).sin() * envelope
            })
            .collect();

        self.beep_sink.clear();
        let source = SamplesBuffer::new(1, sample_rate, samples);
        self.beep_sink.append(source);
        self.beep_sink.play();
        self.beep_sink.sleep_until_end();
    }

    /// Play a short confirmation tune (ascending two-note chirp)
    /// Blocking - waits for tune to finish
    pub fn confirm_beep(&self) {
        let sample_rate = 44100u32;
        let note_ms = 60;
        let gap_ms = 20;
        let freq1 = 880.0f32;
        let freq2 = 1108.73f32;

        let note_samples = (sample_rate as usize * note_ms) / 1000;
        let gap_samples = (sample_rate as usize * gap_ms) / 1000;
        let total_samples = note_samples * 2 + gap_samples;

        let samples: Vec<f32> = (0..total_samples)
            .map(|i| {
                let (frequency, local_i, local_len) = if i < note_samples {
                    (freq1, i, note_samples)
                } else if i < note_samples + gap_samples {
                    return 0.0;
                } else {
                    (freq2, i - note_samples - gap_samples, note_samples)
                };

                let t = local_i as f32 / sample_rate as f32;
                let envelope = if local_i < local_len / 8 {
                    local_i as f32 / (local_len / 8) as f32
                } else if local_i > local_len * 7 / 8 {
                    (local_len - local_i) as f32 / (local_len / 8) as f32
                } else {
                    1.0
                };

                (t * frequency * 2.0 * std::f32::consts::PI).sin() * envelope
            })
            .collect();

        let source = SamplesBuffer::new(1, sample_rate, samples);
        self.beep_sink.append(source);
        self.beep_sink.play();
        self.beep_sink.sleep_until_end();
    }

    /// Play TTS announcement on the voice sink.
    ///
    /// Returns None if completed, Some(event) if interrupted by button press.
    pub fn announce_interruptible(
        &self,
        text: &str,
        interrupt_rx: Option<&std::sync::mpsc::Receiver<ButtonEvent>>,
    ) -> Option<ButtonEvent> {
        info!("TTS: {}", text);

        let raw = match crate::tts::get_audio(text) {
            Some(data) => data,
            None => {
                warn!("No pre-generated audio for: {}", text);
                return None;
            }
        };

        let samples_f32 = crate::tts::pcm_to_f32(raw);
        if samples_f32.is_empty() {
            return None;
        }

        // Clear any previous voice and add a brief gap before speaking
        self.voice_sink.clear();
        let gap = vec![0.0f32; (crate::tts::SAMPLE_RATE as usize) / 2]; // 0.5s silence
        self.voice_sink
            .append(SamplesBuffer::new(1, crate::tts::SAMPLE_RATE, gap));

        let source = SamplesBuffer::new(1, crate::tts::SAMPLE_RATE, samples_f32);
        self.voice_sink.append(source);
        self.voice_sink.play();

        // Wait for playback, checking for interrupts
        while !self.voice_sink.empty() {
            if let Some(rx) = interrupt_rx {
                if let Ok(event) = rx.try_recv() {
                    info!("TTS interrupted by {:?}", event);
                    self.voice_sink.clear();
                    return Some(event);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        None
    }

    /// Play TTS announcement (blocking, no interrupt check)
    pub fn announce(&self, text: &str) {
        self.announce_interruptible(text, None);
    }

    /// Check if voice sink is still playing
    #[allow(dead_code)]
    pub fn is_voice_playing(&self) -> bool {
        !self.voice_sink.empty()
    }

    /// Stop voice playback immediately
    #[allow(dead_code)]
    pub fn stop_voice(&self) {
        self.voice_sink.clear();
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

    let probed =
        match symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts) {
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

/// Reader that fetches HLS segments and presents them as a continuous stream.
///
/// For live streams, the playlist is periodically refreshed to discover new segments.
/// Tracks the next expected media sequence number to avoid re-fetching segments.
struct HlsReader {
    /// Base URL for segments (playlist URL without filename)
    base_url: String,
    /// Full playlist URL (for refreshing)
    playlist_url: String,
    /// Current segment data being read
    current_segment: Cursor<Vec<u8>>,
    /// Pending segment URLs to fetch (only new/unseen segments)
    pending_segments: std::collections::VecDeque<String>,
    /// Next media sequence number we expect (to skip already-played segments on refresh)
    next_media_sequence: u64,
    /// Target duration of each segment (seconds), used for refresh timing
    target_duration: f64,
    /// Stop flag to check
    stop_flag: Arc<AtomicBool>,
}

impl HlsReader {
    fn new(url: &str, stop_flag: Arc<AtomicBool>) -> Result<Self, std::io::Error> {
        let base_url = url
            .rsplit_once('/')
            .map(|(base, _)| format!("{}/", base))
            .unwrap_or_else(|| url.to_string());

        let mut reader = Self {
            base_url,
            playlist_url: url.to_string(),
            current_segment: Cursor::new(Vec::new()),
            pending_segments: std::collections::VecDeque::new(),
            next_media_sequence: 0,
            target_duration: 6.0,
            stop_flag,
        };

        // Fetch initial playlist
        reader.refresh_playlist()?;

        // Pre-fetch first segment
        reader.fetch_next_segment()?;

        Ok(reader)
    }

    /// Refresh playlist and enqueue only new (unseen) segments
    fn refresh_playlist(&mut self) -> Result<(), std::io::Error> {
        debug!("HLS: refreshing playlist: {}", self.playlist_url);

        let response = ureq::get(&self.playlist_url)
            .set("User-Agent", "lego-radio/1.0")
            .call()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut body = String::new();
        response.into_reader().read_to_string(&mut body)?;

        let parsed = m3u8_rs::parse_playlist_res(body.as_bytes()).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{:?}", e))
        })?;

        match parsed {
            m3u8_rs::Playlist::MediaPlaylist(playlist) => {
                let media_sequence = playlist.media_sequence;
                self.target_duration = playlist.target_duration as f64;

                // Each segment has a sequence number: media_sequence + index
                // Only enqueue segments we haven't seen yet
                for (i, segment) in playlist.segments.iter().enumerate() {
                    let seq = media_sequence + i as u64;
                    if seq < self.next_media_sequence {
                        continue; // Already played this segment
                    }

                    let segment_url = if segment.uri.starts_with("http") {
                        segment.uri.clone()
                    } else {
                        format!("{}{}", self.base_url, segment.uri)
                    };
                    self.pending_segments.push_back(segment_url);
                    self.next_media_sequence = seq + 1;
                }

                debug!(
                    "HLS: playlist seq={}, {} new segments queued",
                    media_sequence,
                    self.pending_segments.len()
                );
            }
            m3u8_rs::Playlist::MasterPlaylist(master) => {
                if let Some(variant) = master.variants.first() {
                    let variant_url = if variant.uri.starts_with("http") {
                        variant.uri.clone()
                    } else {
                        format!("{}{}", self.base_url, variant.uri)
                    };
                    debug!("HLS: following master playlist to {}", variant_url);
                    self.playlist_url = variant_url.clone();
                    self.base_url = variant_url
                        .rsplit_once('/')
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
        // If no pending segments, refresh the playlist
        if self.pending_segments.is_empty() {
            // Wait ~half the target duration before refreshing (HLS best practice)
            let wait_ms = (self.target_duration * 500.0) as u64;
            debug!("HLS: no segments, waiting {}ms before refresh", wait_ms);
            std::thread::sleep(Duration::from_millis(wait_ms.max(500)));

            self.refresh_playlist()?;

            // Still nothing? Wait again and retry once more
            if self.pending_segments.is_empty() {
                std::thread::sleep(Duration::from_millis(wait_ms.max(500)));
                self.refresh_playlist()?;
            }

            if self.pending_segments.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "No more segments after refresh",
                ));
            }
        }

        let segment_url = self.pending_segments.pop_front().unwrap();
        debug!("HLS: fetching segment (seq {})", self.next_media_sequence - self.pending_segments.len() as u64 - 1);

        let response = ureq::get(&segment_url)
            .set("User-Agent", "lego-radio/1.0")
            .call()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut data = Vec::new();
        response.into_reader().read_to_end(&mut data)?;

        // Check if this is a TS segment (starts with 0x47 sync byte)
        let audio_data = if data.len() >= 188 && data[0] == 0x47 {
            debug!("HLS: demuxing TS segment ({} bytes)", data.len());
            demux_ts_audio(&data)
        } else {
            data
        };

        self.current_segment = Cursor::new(audio_data);
        Ok(())
    }
}

/// Demux MPEG-TS data to extract audio (AAC) payload
///
/// TS packets are 188 bytes with sync byte 0x47.
/// Audio is typically on PID 0x101 (257) or similar.
/// This extracts PES payload from audio packets.
fn demux_ts_audio(ts_data: &[u8]) -> Vec<u8> {
    let mut audio_data = Vec::new();
    let mut audio_pid: Option<u16> = None;

    // Process TS packets (188 bytes each)
    for chunk in ts_data.chunks(188) {
        if chunk.len() < 188 || chunk[0] != 0x47 {
            continue; // Skip invalid packets
        }

        // Parse TS header
        let pid = (((chunk[1] & 0x1F) as u16) << 8) | (chunk[2] as u16);
        let payload_start = (chunk[1] & 0x40) != 0;
        let has_adaptation = (chunk[3] & 0x20) != 0;
        let has_payload = (chunk[3] & 0x10) != 0;

        if !has_payload {
            continue;
        }

        // Calculate payload offset
        let mut offset = 4;
        if has_adaptation {
            let adaptation_len = chunk[4] as usize;
            offset += 1 + adaptation_len;
        }

        if offset >= 188 {
            continue;
        }

        // For PAT (PID 0), we could parse to find audio PID
        // For simplicity, assume audio is on common PIDs or any PES with audio
        if pid == 0 {
            // PAT - could parse to find PMT, but skip for simplicity
            continue;
        }

        // Check if this looks like audio (common audio PIDs or detect from stream)
        // BBC typically uses PID 0x22 (34) or similar for audio
        let is_audio_pid =
            audio_pid.map(|p| p == pid).unwrap_or(false) || (0x20..0x1FFF).contains(&pid);

        if !is_audio_pid && audio_pid.is_some() {
            continue;
        }

        let payload = &chunk[offset..];

        // Check for PES header (starts with 0x00 0x00 0x01)
        if payload_start
            && payload.len() >= 9
            && payload[0] == 0x00
            && payload[1] == 0x00
            && payload[2] == 0x01
        {
            let stream_id = payload[3];

            // Audio stream IDs: 0xC0-0xDF (MPEG audio), 0xBD (private/AAC)
            if (0xC0..=0xDF).contains(&stream_id) || stream_id == 0xBD {
                if audio_pid.is_none() {
                    audio_pid = Some(pid);
                    debug!("HLS: found audio on PID {}", pid);
                }

                // Skip PES header to get to audio data
                let pes_header_len = 9 + payload[8] as usize;
                if pes_header_len < payload.len() {
                    audio_data.extend_from_slice(&payload[pes_header_len..]);
                }
                continue;
            }
        }

        // Continuation of audio PES
        if audio_pid == Some(pid) {
            audio_data.extend_from_slice(payload);
        }
    }

    debug!("HLS: extracted {} bytes of audio from TS", audio_data.len());
    audio_data
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
    #[allow(clippy::assertions_on_constants)]
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
