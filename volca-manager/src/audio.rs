//! Audio loading, resampling, and playback functionality.

use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, error};

/// Stores original sample data for lossless resampling across different rates.
#[derive(Clone)]
pub struct OriginalSample {
    /// Original samples at original sample rate
    pub samples: Vec<i16>,
    /// Original sample rate (e.g., 44100, 16000)
    pub original_sample_rate: u32,
    /// Current playback speed (for display rate)
    pub current_speed: u16,
}

impl OriginalSample {
    /// Get current effective sample rate in Hz
    pub fn current_sample_rate(&self) -> f32 {
        31250.0 * self.current_speed as f32 / 16384.0
    }

    /// Resample to target rate, returning (resampled_samples, new_speed)
    pub fn resample_to(&self, target_rate: u32) -> Result<(Vec<i16>, u16), String> {
        use volsa2_core::audio::VOLCA_SAMPLERATE;

        // Calculate speed parameter for target rate
        let speed = ((target_rate as f32 / VOLCA_SAMPLERATE as f32) * 16384.0) as u16;

        // If target rate == original rate, no resampling needed
        if target_rate == self.original_sample_rate {
            return Ok((self.samples.clone(), speed));
        }

        // Resample from original to target rate
        let resampled = resample_audio(&self.samples, self.original_sample_rate, VOLCA_SAMPLERATE)
            .map_err(|e| format!("Resample failed: {}", e))?;

        Ok((resampled, speed))
    }
}

/// Load WAV file and store at original sample rate.
/// Returns (original_sample_rate, samples_at_original_rate, speed_for_original_rate)
pub fn load_wav_file(path: &std::path::Path) -> Result<(u32, Vec<i16>, u16), String> {
    use hound::SampleFormat;
    use volsa2_core::audio::VOLCA_SAMPLERATE;

    let mut wav_reader = hound::WavReader::open(path)
        .map_err(|e| format!("Failed to open WAV: {}", e))?;
    let spec = wav_reader.spec();
    let original_sample_rate = spec.sample_rate;
    let channels = spec.channels;

    // Read samples based on format and convert to mono i16 at original rate
    let samples: Vec<i16> = match (spec.sample_format, spec.bits_per_sample, channels) {
        // 16-bit int mono
        (SampleFormat::Int, 16, 1) => {
            wav_reader
                .samples::<i16>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to read samples: {}", e))?
        }
        // 16-bit int stereo - mix to mono
        (SampleFormat::Int, 16, 2) => {
            let all_samples: Vec<i16> = wav_reader
                .samples::<i16>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            all_samples
                .chunks_exact(2)
                .map(|chunk| ((chunk[0] as i32 + chunk[1] as i32) / 2) as i16)
                .collect()
        }
        // 32-bit float mono - convert to i16
        (SampleFormat::Float, 32, 1) => wav_reader
            .samples::<f32>()
            .map(|s| s.map(|f| (f * i16::MAX as f32).round() as i16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to read samples: {}", e))?,
        // 32-bit float stereo - mix to mono
        (SampleFormat::Float, 32, 2) => {
            let all_samples: Vec<f32> = wav_reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            all_samples
                .chunks_exact(2)
                .map(|chunk| (((chunk[0] + chunk[1]) / 2.0) * i16::MAX as f32).round() as i16)
                .collect()
        }
        // Unsupported format
        _ => {
            return Err(format!(
                "Unsupported WAV format: {} bit {:?}, {} channels",
                spec.bits_per_sample, spec.sample_format, channels
            ));
        }
    };

    // Calculate speed for original rate
    let speed = if original_sample_rate > VOLCA_SAMPLERATE {
        16384 // Will resample to 31.25kHz, play at 1x
    } else {
        ((original_sample_rate as f32 / VOLCA_SAMPLERATE as f32) * 16384.0) as u16
    };

    Ok((original_sample_rate, samples, speed))
}

/// Resample audio from source rate to target rate.
pub fn resample_audio(samples: &[i16], from_rate: u32, to_rate: u32) -> Result<Vec<i16>, String> {
    use rubato::{FftFixedIn, Resampler};

    if from_rate == to_rate {
        return Ok(samples.to_vec());
    }

    // Convert i16 samples to f64 for rubato
    let samples_f64: Vec<f64> = samples.iter().map(|&s| s as f64 / i16::MAX as f64).collect();

    // Create resampler
    let mut resampler = FftFixedIn::new(
        from_rate as usize,
        to_rate as usize,
        samples.len(),
        samples.len(),
        1, // mono
    )
    .map_err(|e| format!("Failed to create resampler: {}", e))?;

    // Resample
    let result = resampler
        .process(&[samples_f64], None)
        .map_err(|e| format!("Resampling failed: {}", e))?
        .pop()
        .unwrap();

    // Convert back to i16
    Ok(result
        .into_iter()
        .map(|s| (s * i16::MAX as f64).round() as i16)
        .collect())
}

/// Play audio samples using rodio with speed-adjusted sample rate.
/// Returns true if playback completed, false if stopped early.
pub fn play_audio_sample(samples: &[i16], speed: u16, stop_receiver: mpsc::Receiver<()>) -> bool {
    use rodio::buffer::SamplesBuffer;
    use rodio::{OutputStream, Sink};

    debug!(
        "play_audio_sample called with {} samples, speed {}",
        samples.len(),
        speed
    );

    // Create output stream
    let (_stream, stream_handle) = match OutputStream::try_default() {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create audio output: {}", e);
            return false;
        }
    };
    let sink = match Sink::try_new(&stream_handle) {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to create audio sink: {}", e);
            return false;
        }
    };

    // Volca Sample 2 base specs: 31,250 Hz, mono, 16-bit
    // Speed is a fixed-point value where 16384 = 1.0x
    const BASE_RATE: u32 = 31250;
    const DEFAULT_SPEED: u16 = 16384;
    const CHANNELS: u16 = 1;

    // Calculate effective sample rate based on speed metadata
    let sample_rate = (BASE_RATE as f64 * (speed as f64 / DEFAULT_SPEED as f64)) as u32;

    debug!(
        "Creating SamplesBuffer with {} samples at {} Hz (speed {})",
        samples.len(),
        sample_rate,
        speed
    );

    // Create audio buffer from i16 samples
    let buffer = SamplesBuffer::new(CHANNELS, sample_rate, samples.to_vec());

    // Add to sink and play
    sink.append(buffer);
    debug!("Starting playback...");

    // Poll for completion or stop signal
    loop {
        // Check if playback finished
        if sink.empty() {
            debug!("Playback complete");
            return true;
        }

        // Check for stop signal (non-blocking)
        match stop_receiver.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                debug!("Stop signal received");
                sink.stop();
                return false;
            }
            Err(mpsc::TryRecvError::Empty) => {
                // Continue playing
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
