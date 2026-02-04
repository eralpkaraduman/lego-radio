use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, Sink};
use std::io::Read;
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
const VOLUME: f32 = 0.8;

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
                (t * frequency * 2.0 * std::f32::consts::PI).sin() * 0.3 * envelope
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

    // Make HTTP request
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

    // Signal connected
    let _ = connected_tx.send(true);

    // Create a reader that checks stop flag
    let reader = StoppableReader::new(response.into_reader(), stop_flag.clone());
    let mss = MediaSourceStream::new(Box::new(reader), Default::default());

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
