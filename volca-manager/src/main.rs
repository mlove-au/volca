slint::include_modules!();

mod midi;

use midi::MidiDevice;
use slint::{Model, VecModel, SharedString};
use std::rc::Rc;
use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Generate SVG path commands for waveform display (envelope style)
/// Coordinates normalized to viewbox dimensions: x in [0, width], y in [0, height]
/// Sample value +1 maps to y=0 (top), -1 maps to y=height (bottom), 0 at center
/// Creates a filled envelope showing min/max values at each point
fn generate_waveform_path(samples: &[i16], num_points: usize, width: f64, height: f64) -> String {
    if samples.is_empty() {
        let center_y = height / 2.0;
        return format!("M 0 {} L {} {} L {} {} L 0 {} Z", center_y, width, center_y, width, center_y, center_y);
    }

    let num_points = num_points.max(2);  // Need at least 2 points

    // We'll draw the upper envelope forward, then lower envelope backward to create a filled shape
    let mut upper_points: Vec<(f64, f64)> = Vec::with_capacity(num_points);
    let mut lower_points: Vec<(f64, f64)> = Vec::with_capacity(num_points);

    for i in 0..num_points {
        // Map i to sample range
        let t = i as f64 / (num_points - 1) as f64;  // 0.0 to 1.0
        let center_idx = (t * (samples.len() - 1) as f64) as usize;

        // Sample a window around this position
        let window_size = (samples.len() / num_points).max(1);
        let start_idx = center_idx.saturating_sub(window_size / 2);
        let end_idx = (start_idx + window_size).min(samples.len());

        // Find min/max sample values in this window
        let mut min_val: i16 = i16::MAX;
        let mut max_val: i16 = i16::MIN;
        for j in start_idx..end_idx {
            let s = samples[j];
            min_val = min_val.min(s);
            max_val = max_val.max(s);
        }

        // Convert to viewbox coordinates
        // x: 0 to width (full width)
        // y: 0 (top, +1.0) to height (bottom, -1.0), center at height/2
        let x = t * width;
        let center_y = height / 2.0;
        let y_range = height * 0.98 / 2.0;  // Use 98% of height for waveform (slight padding)
        let y_max = center_y - (max_val as f64 / i16::MAX as f64) * y_range;
        let y_min = center_y - (min_val as f64 / i16::MAX as f64) * y_range;

        upper_points.push((x, y_max));
        lower_points.push((x, y_min));
    }

    // Build path: upper envelope forward, then lower envelope backward
    let mut path = String::with_capacity(num_points * 40);

    // Start at x=0 with first point
    path.push_str(&format!("M 0 {:.1}", upper_points.first().map(|p| p.1).unwrap_or(500.0)));

    // Draw upper envelope
    for &(x, y) in upper_points.iter() {
        path.push_str(&format!(" L {:.1} {:.1}", x, y));
    }

    // Ensure we reach x=width
    if let Some(&(_, y)) = upper_points.last() {
        path.push_str(&format!(" L {:.1} {:.1}", width, y));
    }

    // Draw lower envelope in reverse
    if let Some(&(_, y)) = lower_points.last() {
        path.push_str(&format!(" L {:.1} {:.1}", width, y));
    }
    for &(x, y) in lower_points.iter().rev() {
        path.push_str(&format!(" L {:.1} {:.1}", x, y));
    }

    // Close back to start
    path.push_str(&format!(" L 0 {:.1}", lower_points.first().map(|p| p.1).unwrap_or(500.0)));
    path.push_str(" Z");

    path
}

/// Load WAV file and prepare for Volca Sample 2
/// Returns (samples, speed, sample_rate_khz) or error
fn load_wav_file(path: &std::path::Path) -> Result<(Vec<i16>, u16, f32), String> {
    use volsa2_core::audio::{AudioReader, VOLCA_SAMPLERATE};

    // First, read the WAV spec to get original sample rate
    let wav_reader = hound::WavReader::open(path)
        .map_err(|e| format!("Failed to open WAV: {}", e))?;
    let spec = wav_reader.spec();
    let original_sample_rate = spec.sample_rate;
    drop(wav_reader);  // Close the reader so AudioReader can open it

    // Now use AudioReader for proper processing
    let audio = AudioReader::open_file(path)
        .map_err(|e| format!("Failed to open WAV: {}", e))?;

    let channels = audio.channels();

    // Convert to mono and resample as needed
    let samples = match channels {
        1 => audio.resample_to_volca(),  // Already mono
        _ => audio.take_mid().resample_to_volca(),  // Convert stereo to mono (L+R)/2
    }
    .map_err(|e| format!("Failed to process audio: {}", e))?;

    // Calculate speed parameter for playback
    // speed = (original_rate / 31250) * 16384
    // If we resampled DOWN (original > 31.25kHz), speed stays at 16384 (1.0x)
    // If original was lower, speed adjusts for slower playback
    let speed = if original_sample_rate > VOLCA_SAMPLERATE {
        16384  // Resampled to 31.25kHz, play at 1x
    } else {
        // Calculate speed for lower sample rates
        ((original_sample_rate as f32 / VOLCA_SAMPLERATE as f32) * 16384.0) as u16
    };

    // Calculate display sample rate in kHz
    let display_rate_khz = 31.25 * speed as f32 / 16384.0;

    Ok((samples, speed, display_rate_khz))
}

pub struct AppState {
    pub samples: Rc<VecModel<SampleInfo>>,
    pub connected: bool,
    pub device: Arc<Mutex<Option<MidiDevice>>>,  // Arc for thread-safety
    pub firmware_version: String,
    pub logs: Rc<VecModel<SharedString>>,
    pub ui: slint::Weak<AppWindow>,
    pub downloaded_samples: Arc<Mutex<HashMap<u8, (Vec<i16>, u16)>>>,  // Arc for thread-safety (audio_data, speed)
    pub stop_sender: Arc<Mutex<Option<Sender<()>>>>,  // Arc for thread-safety
    pub is_playing: Arc<AtomicBool>,  // Thread-safe playback state
    pub playback_info: Arc<Mutex<Option<(Instant, f64)>>>,  // (start_time, duration_secs)
}

impl AppState {
    const MAX_LOGS: usize = 100; // Keep last 100 messages

    pub fn log(&self, message: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S");
        let log_msg = format!("[{}] {}", timestamp, message);

        // Add to logs
        self.logs.push(SharedString::from(log_msg));

        // Trim if too many
        if self.logs.row_count() > Self::MAX_LOGS {
            let to_remove = self.logs.row_count() - Self::MAX_LOGS;
            for _ in 0..to_remove {
                // Remove from beginning (oldest)
                // Note: VecModel doesn't have remove_front, so we'll just let it grow for now
            }
        }

        // Update UI with joined text
        self.update_log_text_ui();

        // Also print to console
        println!("{}", message);
    }

    fn update_log_text_ui(&self) {
        if let Some(ui) = self.ui.upgrade() {
            // Join all logs with newlines
            let log_text: String = (0..self.logs.row_count())
                .filter_map(|i| self.logs.row_data(i))
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join("\n");

            ui.set_log_text(SharedString::from(log_text));
        }
    }

    pub fn new(ui: slint::Weak<AppWindow>) -> Self {
        let samples = Rc::new(VecModel::default());

        // Initialize with 200 empty slots
        for i in 0..200 {
            samples.push(SampleInfo {
                slot: i,
                name: SharedString::from(""),
                length: 0,
                speed: 0,
                has_sample: false,
                name_edited: false,
            });
        }

        Self {
            samples,
            connected: false,
            device: Arc::new(Mutex::new(None)),
            firmware_version: String::new(),
            logs: Rc::new(VecModel::default()),
            ui,
            downloaded_samples: Arc::new(Mutex::new(HashMap::new())),
            stop_sender: Arc::new(Mutex::new(None)),
            is_playing: Arc::new(AtomicBool::new(false)),
            playback_info: Arc::new(Mutex::new(None)),
        }
    }

    /// Scan all 200 sample slots from the device
    pub fn scan_all_samples(&self) {
        self.log("Starting scan of all 200 sample slots...".to_string());

        if let Some(ui) = self.ui.upgrade() {
            ui.set_scanning(true);
        }

        let mut device_opt = self.device.lock().unwrap();
        if let Some(device) = device_opt.as_mut() {
            let mut samples_found = 0;

            for slot in 0..200 {
                // Log progress every 20 slots
                if slot % 20 == 0 {
                    self.log(format!("Scanning slots {}-{}...", slot, (slot + 19).min(199)));
                }

                match device.get_sample_info(slot as u8) {
                    Ok(header) => {
                        let has_sample = header.length > 0;
                        if has_sample {
                            samples_found += 1;
                        }

                        self.samples.set_row_data(slot as usize, SampleInfo {
                            slot: slot as i32,
                            name: SharedString::from(if has_sample { header.name.as_str() } else { "(empty)" }),
                            length: header.length as i32,
                            speed: header.speed as i32,
                            has_sample,
                            name_edited: false,
                        });

                        // Update UI periodically
                        if let Some(ui) = self.ui.upgrade() {
                            ui.set_samples(self.samples.clone().into());
                        }
                    }
                    Err(e) => {
                        self.log(format!("Error reading slot {}: {}", slot, e));
                        // Set as empty on error
                        self.samples.set_row_data(slot as usize, SampleInfo {
                            slot: slot as i32,
                            name: SharedString::from("(error)"),
                            length: 0,
                            speed: 0,
                            has_sample: false,
                            name_edited: false,
                        });
                    }
                }
            }

            self.log(format!("Scan complete! Found {} samples in 200 slots.", samples_found));
        } else {
            self.log("Error: No device connected".to_string());
        }

        if let Some(ui) = self.ui.upgrade() {
            ui.set_scanning(false);
        }
    }

    pub fn update_sample(&mut self, slot: i32, info: SampleInfo) {
        if slot >= 0 && slot < 200 {
            self.samples.set_row_data(slot as usize, info);
        }
    }

    pub fn get_sample(&self, slot: i32) -> Option<SampleInfo> {
        if slot >= 0 && (slot as usize) < self.samples.row_count() {
            self.samples.row_data(slot as usize)
        } else {
            None
        }
    }
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            samples: self.samples.clone(),
            connected: self.connected,
            device: self.device.clone(),
            firmware_version: self.firmware_version.clone(),
            logs: self.logs.clone(),
            ui: self.ui.clone(),
            downloaded_samples: self.downloaded_samples.clone(),
            stop_sender: self.stop_sender.clone(),
            is_playing: self.is_playing.clone(),
            playback_info: self.playback_info.clone(),
        }
    }
}

/// Play audio samples using rodio with speed-adjusted sample rate
/// Returns true if playback completed, false if stopped early
fn play_audio_sample(samples: &[i16], speed: u16, stop_receiver: mpsc::Receiver<()>) -> bool {
    use rodio::{OutputStream, Sink};
    use rodio::buffer::SamplesBuffer;
    use std::time::Duration;

    println!("DEBUG: play_audio_sample called with {} samples, speed {}", samples.len(), speed);

    // Create output stream
    let (_stream, stream_handle) = match OutputStream::try_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to create audio output: {}", e);
            return false;
        }
    };
    let sink = match Sink::try_new(&stream_handle) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to create audio sink: {}", e);
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

    println!("DEBUG: Creating SamplesBuffer with {} samples at {} Hz (speed {})", samples.len(), sample_rate, speed);

    // Create audio buffer from i16 samples
    let buffer = SamplesBuffer::new(CHANNELS, sample_rate, samples.to_vec());

    // Add to sink and play
    sink.append(buffer);
    println!("DEBUG: Starting playback...");

    // Poll for completion or stop signal
    loop {
        // Check if playback finished
        if sink.empty() {
            println!("DEBUG: Playback complete");
            return true;
        }

        // Check for stop signal (non-blocking)
        match stop_receiver.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                println!("DEBUG: Stop signal received");
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

fn main() -> Result<(), slint::PlatformError> {
    // Initialize tracing for debugging
    tracing_subscriber::fmt::init();

    // Create the Slint UI
    let ui = AppWindow::new()?;

    // Create weak reference for AppState
    let ui_weak = ui.as_weak();

    // Create application state with UI reference
    let app_state = AppState::new(ui_weak);

    // Set the sample model in the UI
    ui.set_samples(app_state.samples.clone().into());
    ui.set_loading_slot(-1);
    ui.set_connected(false);
    ui.set_scanning(false);
    ui.set_current_page(0);
    ui.set_logs(app_state.logs.clone().into());
    ui.set_log_text(SharedString::from(""));

    // Initial log message
    app_state.log("Application started".to_string());

    // Setup callbacks
    setup_callbacks(&ui, app_state.clone());

    // Auto-connect on startup
    app_state.log("Auto-connecting to Volca Sample 2...".to_string());
    match MidiDevice::connect() {
        Ok(device) => {
            let firmware = device.firmware_version.clone();
            *app_state.device.lock().unwrap() = Some(device);
            ui.set_connected(true);
            app_state.log(format!("Successfully connected! Firmware: {}", firmware));

            // Auto-scan samples on connect
            app_state.log("Auto-scanning all sample slots...".to_string());
            app_state.scan_all_samples();
        }
        Err(e) => {
            app_state.log(format!("Auto-connect failed: {}. Click Connect to retry.", e));
            ui.set_connected(false);
        }
    }

    // Timer to sync playback state and position from thread to UI
    let is_playing_timer = app_state.is_playing.clone();
    let playback_info_timer = app_state.playback_info.clone();
    let ui_weak_timer = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(50),  // 50ms for smoother playhead movement
        move || {
            if let Some(ui) = ui_weak_timer.upgrade() {
                let playing = is_playing_timer.load(Ordering::SeqCst);
                if ui.get_is_playing() != playing {
                    ui.set_is_playing(playing);
                    if !playing {
                        // Reset position when playback stops
                        ui.set_playback_position(0.0);
                    }
                }

                // Update playback position if playing
                if playing {
                    if let Some((start_time, duration)) = *playback_info_timer.lock().unwrap() {
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let position = (elapsed / duration).min(1.0);
                        ui.set_playback_position(position as f32);
                    }
                }
            }
        },
    );

    // Run the application
    ui.run()
}

fn setup_callbacks(ui: &AppWindow, state: AppState) {
    let ui_weak = ui.as_weak();

    // Connect/Disconnect button callback
    let ui_weak_connect = ui_weak.clone();
    let state_connect = state.clone();
    ui.on_connect_clicked(move || {
        let ui = ui_weak_connect.unwrap();
        let is_connected = ui.get_connected();

        if is_connected {
            // Disconnect
            state_connect.log("Disconnecting from device...".to_string());
            *state_connect.device.lock().unwrap() = None;
            ui.set_connected(false);
            state_connect.log("Disconnected.".to_string());
        } else {
            // Connect
            state_connect.log("Attempting to connect to Volca Sample 2...".to_string());
            match MidiDevice::connect() {
                Ok(device) => {
                    let firmware = device.firmware_version.clone();
                    *state_connect.device.lock().unwrap() = Some(device);
                    ui.set_connected(true);
                    state_connect.log(format!("Successfully connected! Firmware: {}", firmware));

                    // Auto-scan on connect
                    state_connect.scan_all_samples();
                }
                Err(e) => {
                    state_connect.log(format!("Failed to connect: {}", e));
                    ui.set_connected(false);
                }
            }
        }
    });

    // Scan/Refresh button callback
    let state_refresh = state.clone();
    ui.on_refresh_clicked(move || {
        state_refresh.log("Scan button clicked - scanning all samples...".to_string());
        state_refresh.scan_all_samples();
    });

    // Upload callback
    let ui_weak_upload = ui_weak.clone();
    let state_upload = state.clone();
    ui.on_upload_clicked(move |slot| {
        state_upload.log(format!("Upload to slot {} clicked", slot));
        let _ui = ui_weak_upload.unwrap();
        // TODO: Implement upload
    });

    // Download callback - refresh sample from device with loading state
    let state_download = state.clone();
    let ui_weak_download = ui_weak.clone();
    ui.on_download_clicked(move |slot| {
        // Check if sample exists in UI state
        let sample = match state_download.get_sample(slot) {
            Some(s) if s.has_sample => s,
            _ => {
                state_download.log(format!("Slot {} is empty, nothing to download", slot));
                return;
            }
        };

        let ui = ui_weak_download.unwrap();

        // Stop any active playback
        if let Some(sender) = state_download.stop_sender.lock().unwrap().take() {
            let _ = sender.send(());
        }
        state_download.is_playing.store(false, Ordering::SeqCst);
        *state_download.playback_info.lock().unwrap() = None;
        ui.set_is_playing(false);
        ui.set_playback_position(0.0);

        // Set loading state BEFORE starting download
        ui.set_loading_slot(slot);
        state_download.log(format!("Refreshing slot {}: {}...", slot, sample.name));

        // Spawn background thread for download
        let device_ref = state_download.device.clone();
        let downloaded_ref = state_download.downloaded_samples.clone();
        let ui_weak = state_download.ui.clone();

        std::thread::spawn(move || {
            use std::time::Instant;
            let start_time = Instant::now();

            // Get device and download the sample
            let mut device_guard = device_ref.lock().unwrap();
            if let Some(device) = device_guard.as_mut() {
                // Fetch sample header and data
                match device.get_sample_info(slot as u8) {
                    Ok(header) => {
                        match device.get_sample_data(slot as u8, header.length as usize) {
                            Ok(sample_data) => {
                                let elapsed = start_time.elapsed();

                                // Store in memory
                                let audio_data_clone = sample_data.data.clone();
                                downloaded_ref.lock().unwrap().insert(
                                    slot as u8,
                                    (audio_data_clone.clone(), header.speed)
                                );

                                // Update UI on main thread with fresh sample info
                                let waveform_data = audio_data_clone.clone();
                                let header_name = header.name.clone();
                                let header_length = header.length;
                                let header_speed = header.speed;
                                println!("✓ Refreshed in {:.3}s", elapsed.as_secs_f64());
                                let ui_weak_clone = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_clone.upgrade() {
                                        // Update SampleInfo with fresh data from device (clears any unsaved edits)
                                        ui.invoke_update_sample_from_device(
                                            slot,
                                            SharedString::from(header_name),
                                            header_length as i32,
                                            header_speed as i32
                                        );

                                        // Generate and display waveform
                                        let width = ui.get_waveform_display_width();
                                        let height = ui.get_waveform_display_height();
                                        let waveform_path = generate_waveform_path(&waveform_data, 1000, width as f64, height as f64);
                                        ui.set_waveform_path(SharedString::from(waveform_path));

                                        // Clear loading state
                                        ui.set_loading_slot(-1);
                                    }
                                }).ok();
                            }
                            Err(e) => {
                                let error_msg = format!("✗ Refresh failed: {}", e);
                                println!("{}", error_msg);
                                let ui_weak_clone = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_clone.upgrade() {
                                        ui.set_loading_slot(-1);
                                    }
                                }).ok();
                            }
                        }
                    }
                    Err(e) => {
                        let error_msg = format!("✗ Failed to get sample info: {}", e);
                        println!("{}", error_msg);
                        let ui_weak_clone = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_clone.upgrade() {
                                ui.set_loading_slot(-1);
                            }
                        }).ok();
                    }
                }
            } else {
                println!("ERROR: Device not connected");
                let ui_weak_clone = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_clone.upgrade() {
                        ui.set_loading_slot(-1);
                    }
                }).ok();
            }
        });
    });

    // Play callback - plays the currently selected sample
    let state_play = state.clone();
    let ui_weak_play = ui_weak.clone();
    ui.on_play_clicked(move || {
        let ui = ui_weak_play.unwrap();
        let slot = ui.get_selected_slot();

        if slot < 0 {
            state_play.log("No sample selected".to_string());
            return;
        }

        if let Some(sample) = state_play.get_sample(slot) {
            if sample.has_sample {
                // Check if we have the sample data in memory - clone immediately to avoid holding lock
                let maybe_audio_data = state_play.downloaded_samples.lock().unwrap().get(&(slot as u8)).map(|(data, speed)| (data.clone(), *speed));
                if let Some((audio_data, speed)) = maybe_audio_data {
                    let sample_count = audio_data.len();
                    let effective_rate = (31250.0 * (speed as f64 / 16384.0)) as u32;
                    let duration_secs = sample_count as f64 / effective_rate as f64;
                    state_play.log(format!("Playing {} ({:.1}s)", sample.name, duration_secs));

                    // Create stop channel
                    let (tx, rx) = mpsc::channel();
                    *state_play.stop_sender.lock().unwrap() = Some(tx);

                    // Set playing state and store playback info for position tracking
                    state_play.is_playing.store(true, Ordering::SeqCst);
                    *state_play.playback_info.lock().unwrap() = Some((Instant::now(), duration_secs));
                    ui.set_is_playing(true);
                    ui.set_playback_position(0.0);

                    // Play the audio in a background thread
                    let is_playing_flag = state_play.is_playing.clone();
                    let playback_info_flag = state_play.playback_info.clone();
                    std::thread::spawn(move || {
                        let completed = play_audio_sample(&audio_data, speed, rx);
                        println!("Playback {}", if completed { "complete" } else { "stopped" });
                        is_playing_flag.store(false, Ordering::SeqCst);
                        *playback_info_flag.lock().unwrap() = None;
                    });
                } else {
                    state_play.log(format!("Sample not downloaded yet. Click 💾 to download first."));
                }
            }
        }
    });

    // Stop callback
    let state_stop = state.clone();
    ui.on_stop_clicked(move || {
        if let Some(sender) = state_stop.stop_sender.lock().unwrap().take() {
            let _ = sender.send(());
            state_stop.log("Stopping playback...".to_string());
        }
    });

    // Delete callback
    let state_delete = state.clone();
    ui.on_delete_clicked(move |slot| {
        if let Some(sample) = state_delete.get_sample(slot) {
            if sample.has_sample {
                state_delete.log(format!("Delete slot {}: {}", slot, sample.name));
            }
        }
        // TODO: Implement delete
    });

    // Page navigation callbacks
    let ui_weak_prev = ui_weak.clone();
    ui.on_prev_page_clicked(move || {
        let ui = ui_weak_prev.unwrap();
        let current = ui.get_current_page();
        if current > 0 {
            ui.set_current_page(current - 1);
        }
    });

    let ui_weak_next = ui_weak.clone();
    ui.on_next_page_clicked(move || {
        let ui = ui_weak_next.unwrap();
        let current = ui.get_current_page();
        let total = ui.get_total_pages();
        if current < total - 1 {
            ui.set_current_page(current + 1);
        }
    });

    // Debug log toggle callback (Cmd+D)
    let ui_weak_toggle = ui_weak.clone();
    ui.on_toggle_debug_log(move || {
        println!("DEBUG: toggle_debug_log callback triggered!");
        let ui = ui_weak_toggle.unwrap();
        let current = ui.get_show_debug_log();
        println!("DEBUG: current show_debug_log = {}, setting to {}", current, !current);
        ui.set_show_debug_log(!current);
    });

    // Pad selection callback - select a sample to view its waveform
    let state_select = state.clone();
    ui.on_pad_selected(move |slot| {
        println!("Pad selected: slot {}", slot);
        let ui = state_select.ui.upgrade().unwrap();
        ui.set_selected_slot(slot);

        // Get sample info for the selected slot
        if let Some(sample) = state_select.get_sample(slot) {
            // Do nothing if the slot is empty
            if !sample.has_sample {
                return;
            }

            ui.set_selected_sample_name(sample.name.clone());
            ui.set_selected_sample_length(sample.length as i32);

            // Stop any active playback when switching samples
            if let Some(sender) = state_select.stop_sender.lock().unwrap().take() {
                let _ = sender.send(());
            }
            state_select.is_playing.store(false, Ordering::SeqCst);
            *state_select.playback_info.lock().unwrap() = None;
            ui.set_is_playing(false);
            ui.set_playback_position(0.0);

            // Check if we have downloaded audio data for this slot
            // Clone the data immediately to avoid holding the lock
            let maybe_audio_data = state_select.downloaded_samples.lock().unwrap().get(&(slot as u8)).map(|(data, speed)| (data.clone(), *speed));
                if let Some((audio_data, _speed)) = maybe_audio_data {
                    state_select.log(format!("Selected slot {}: {} ({} samples)", slot, sample.name, audio_data.len()));

                    // Generate SVG path for waveform using current display dimensions
                    let width = ui.get_waveform_display_width();
                    let height = ui.get_waveform_display_height();
                    let waveform_path = generate_waveform_path(&audio_data, 1000, width as f64, height as f64);
                    ui.set_waveform_path(SharedString::from(waveform_path));
                } else {
                    // Sample not downloaded yet - download it automatically
                    state_select.log(format!("Selected slot {}: {} (downloading...)", slot, sample.name));
                    ui.set_waveform_path(SharedString::from(""));

                    // Set loading state for this slot BEFORE starting download
                    ui.set_loading_slot(slot);

                    // Spawn background thread for download to keep UI responsive
                    let device_ref = state_select.device.clone();
                    let downloaded_ref = state_select.downloaded_samples.clone();
                    let ui_weak = state_select.ui.clone();

                    std::thread::spawn(move || {
                        use std::time::Instant;
                        let start_time = Instant::now();

                        // Get device and download the sample
                        let mut device_guard = device_ref.lock().unwrap();
                        if let Some(device) = device_guard.as_mut() {
                            // Fetch sample header and data
                            match device.get_sample_info(slot as u8) {
                                Ok(header) => {
                                    match device.get_sample_data(slot as u8, header.length as usize) {
                                        Ok(sample_data) => {
                                            let elapsed = start_time.elapsed();

                                            // Store in memory
                                            let audio_data_clone = sample_data.data.clone();
                                            downloaded_ref.lock().unwrap().insert(
                                                slot as u8,
                                                (audio_data_clone.clone(), header.speed)
                                            );

                                            // Update UI on main thread
                                            let waveform_data = audio_data_clone.clone();
                                            println!("✓ Downloaded in {:.3}s", elapsed.as_secs_f64());
                                            let ui_weak_clone = ui_weak.clone();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_weak_clone.upgrade() {
                                                    // Generate and display waveform
                                                    let width = ui.get_waveform_display_width();
                                                    let height = ui.get_waveform_display_height();
                                                    let waveform_path = generate_waveform_path(&waveform_data, 1000, width as f64, height as f64);
                                                    ui.set_waveform_path(SharedString::from(waveform_path));

                                                    // Clear loading state
                                                    ui.set_loading_slot(-1);
                                                }
                                            }).ok();
                                        }
                                        Err(e) => {
                                            let error_msg = format!("✗ Download failed: {}", e);
                                            println!("{}", error_msg);
                                            let ui_weak_clone = ui_weak.clone();
                                            slint::invoke_from_event_loop(move || {
                                                if let Some(ui) = ui_weak_clone.upgrade() {
                                                    ui.set_loading_slot(-1);
                                                }
                                            }).ok();
                                        }
                                    }
                                }
                                Err(e) => {
                                    let error_msg = format!("✗ Failed to get sample info: {}", e);
                                    println!("{}", error_msg);
                                    let ui_weak_clone = ui_weak.clone();
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_weak_clone.upgrade() {
                                            ui.set_loading_slot(-1);
                                        }
                                    }).ok();
                                }
                            }
                        } else {
                            println!("ERROR: Device not connected");
                            let ui_weak_clone = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_clone.upgrade() {
                                    ui.set_loading_slot(-1);
                                }
                            }).ok();
                        }
                    });
                }

            // Reset zoom and scroll when selecting a new sample
            ui.set_waveform_zoom(1.0);
            ui.set_waveform_scroll(0.0);
        }
    });

    // Regenerate waveform when display size changes
    let state_regen = state.clone();
    ui.on_regenerate_waveform(move |width, height| {
        let ui = state_regen.ui.upgrade().unwrap();
        let slot = ui.get_selected_slot();

        // Only regenerate if we have a sample selected
        if let Some(sample) = state_regen.get_sample(slot) {
            if sample.has_sample {
                // Clone the data immediately to avoid holding the lock
                let maybe_audio_data = state_regen.downloaded_samples.lock().unwrap().get(&(slot as u8)).map(|(data, speed)| (data.clone(), *speed));
                if let Some((audio_data, _speed)) = maybe_audio_data {
                    // Regenerate waveform with new dimensions
                    let waveform_path = generate_waveform_path(&audio_data, 1000, width as f64, height as f64);
                    ui.set_waveform_path(SharedString::from(waveform_path));
                }
            }
        }
    });

    // Load WAV callback - replace sample audio without writing to device
    let state_load_wav = state.clone();
    let ui_weak_load_wav = ui_weak.clone();
    ui.on_load_wav_clicked(move || {
        let ui = ui_weak_load_wav.unwrap();
        let slot = ui.get_selected_slot();

        if slot < 0 || slot >= 200 {
            println!("No slot selected for WAV load");
            return;
        }

        // Open file dialog (rfd already in Cargo.toml)
        let file = rfd::FileDialog::new()
            .add_filter("WAV Audio", &["wav", "WAV"])
            .set_title("Select WAV file to load")
            .pick_file();

        let Some(path) = file else {
            println!("No file selected");
            return;
        };

        println!("Loading WAV: {:?}", path);

        // Load and process WAV file
        match load_wav_file(&path) {
            Ok((samples, speed, display_rate_khz)) => {
                let sample_count = samples.len();
                let duration_secs = sample_count as f32 / 31250.0;

                println!("Loaded {} samples ({:.2}s) at {:.2} kHz",
                         sample_count, duration_secs, display_rate_khz);

                // Extract filename for display
                let filename = path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .to_string();

                // Truncate to 24 chars (Volca limit)
                let name = if filename.len() > 24 {
                    filename.chars().take(24).collect()
                } else {
                    filename
                };

                // Store in downloaded_samples (in-memory only, not on device)
                state_load_wav.downloaded_samples.lock().unwrap().insert(
                    slot as u8,
                    (samples.clone(), speed)
                );

                // Update SampleInfo with new metadata and mark as edited
                let updated_info = SampleInfo {
                    slot,
                    name: SharedString::from(&name),
                    length: sample_count as i32,
                    speed: speed as i32,
                    has_sample: true,
                    name_edited: true,  // Mark as edited (shows ✏️)
                };
                state_load_wav.samples.set_row_data(slot as usize, updated_info);

                // Regenerate waveform display
                let width = ui.get_waveform_display_width();
                let height = ui.get_waveform_display_height();
                let waveform_path = generate_waveform_path(&samples, 1000, width as f64, height as f64);

                // Update UI
                ui.set_selected_sample_name(SharedString::from(&name));
                ui.set_selected_sample_length(sample_count as i32);
                ui.set_waveform_path(SharedString::from(waveform_path));
                ui.set_samples(state_load_wav.samples.clone().into());

                state_load_wav.log(format!("Loaded WAV: {} ({:.2}s, {:.2} kHz) - not saved to device",
                                           name, duration_secs, display_rate_khz));
            }
            Err(e) => {
                println!("ERROR loading WAV: {}", e);
                state_load_wav.log(format!("Failed to load WAV: {}", e));
            }
        }
    });

    // Name edit accepted callback - update name and set edited flag
    let state_name_edit = state.clone();
    ui.on_name_edit_accepted(move |slot, new_name| {
        let new_name_str = new_name.to_string();

        state_name_edit.log(format!("Name edited for slot {}: \"{}\" (not yet saved)", slot, new_name_str));

        // Update UI with new name and set edited flag
        if let Some(mut sample) = state_name_edit.get_sample(slot) {
            sample.name = SharedString::from(&new_name_str);
            sample.name_edited = true;
            state_name_edit.samples.set_row_data(slot as usize, sample);

            // Update UI
            if let Some(ui) = state_name_edit.ui.upgrade() {
                ui.set_samples(state_name_edit.samples.clone().into());
            }
        }
    });

    // Clear name edited flag callback - called from background thread via invoke_from_event_loop
    let state_clear_flag = state.clone();
    ui.on_clear_name_edited_flag(move |slot| {
        if let Some(mut sample) = state_clear_flag.get_sample(slot) {
            sample.name_edited = false;
            state_clear_flag.samples.set_row_data(slot as usize, sample);

            if let Some(ui) = state_clear_flag.ui.upgrade() {
                ui.set_samples(state_clear_flag.samples.clone().into());
            }
        }
    });

    // Update sample from device callback - used after refresh to sync with device state
    let state_update_sample = state.clone();
    ui.on_update_sample_from_device(move |slot, name, length, speed| {
        let updated_info = SampleInfo {
            slot,
            name,
            length,
            speed,
            has_sample: true,
            name_edited: false,  // Clear edited flag when refreshing from device
        };
        state_update_sample.samples.set_row_data(slot as usize, updated_info);

        if let Some(ui) = state_update_sample.ui.upgrade() {
            ui.set_samples(state_update_sample.samples.clone().into());
        }
    });

    // Save to device callback - write SampleHeader back to device
    let state_save = state.clone();
    let ui_weak_save = ui_weak.clone();
    ui.on_save_to_device_clicked(move |slot| {
        // Get current sample info
        let sample = match state_save.get_sample(slot) {
            Some(s) if s.has_sample => s,
            _ => {
                state_save.log(format!("Cannot save slot {}: no sample exists", slot));
                return;
            }
        };

        let ui = ui_weak_save.unwrap();

        // Set loading state BEFORE starting save
        ui.set_loading_slot(slot);
        state_save.log(format!("Saving sample info for slot {}: \"{}\"...", slot, sample.name));

        // Spawn background thread for save operation
        let device_ref = state_save.device.clone();
        let downloaded_ref = state_save.downloaded_samples.clone();
        let ui_weak = state_save.ui.clone();
        let sample_name = sample.name.to_string();
        let sample_length = sample.length as u32;
        let sample_speed = sample.speed as u16;

        std::thread::spawn(move || {
            use std::time::{Instant, Duration};
            use volsa2_core::proto::{SampleHeader, SampleData, Status};

            let start_time = Instant::now();

            // Check if sample is downloaded (need audio data to persist name change)
            let audio_data = match downloaded_ref.lock().unwrap().get(&(slot as u8)) {
                Some((data, _)) => data.clone(),
                None => {
                    println!("ERROR: Sample not downloaded. Download first before saving name changes.");
                    let ui_weak_clone = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_clone.upgrade() {
                            ui.set_loading_slot(-1);
                        }
                    }).ok();
                    return;
                }
            };

            // Get device and send both header and data
            let mut device_guard = device_ref.lock().unwrap();
            if let Some(device) = device_guard.as_mut() {
                // Create SampleHeader with updated name
                let header = SampleHeader {
                    sample_no: slot as u8,
                    name: sample_name.clone(),
                    length: sample_length,
                    level: 65535,  // SampleHeader::DEFAULT_LEVEL
                    speed: sample_speed,
                };

                // Create SampleData with unchanged audio
                let data = SampleData {
                    sample_no: slot as u8,
                    data: audio_data,
                };

                // Send header and wait for ACK, then data and wait for ACK
                match device.send_message(&header) {
                    Ok(()) => {
                        // Wait for ACK after header
                        match device.receive_message::<Status>(Duration::from_millis(500)) {
                            Ok(status) => {
                                if let Err(e) = status {
                                    println!("✗ Device NAK after header: {}", e);
                                    let ui_weak_clone = ui_weak.clone();
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_weak_clone.upgrade() {
                                            ui.set_loading_slot(-1);
                                        }
                                    }).ok();
                                    return;
                                }

                                // Header ACK received, now send data
                                match device.send_message(&data) {
                                    Ok(()) => {
                                        // Wait for ACK after data
                                        match device.receive_message::<Status>(Duration::from_millis(500)) {
                                            Ok(status) => {
                                                if let Err(e) = status {
                                                    println!("✗ Device NAK after data: {}", e);
                                                    let ui_weak_clone = ui_weak.clone();
                                                    slint::invoke_from_event_loop(move || {
                                                        if let Some(ui) = ui_weak_clone.upgrade() {
                                                            ui.set_loading_slot(-1);
                                                        }
                                                    }).ok();
                                                    return;
                                                }

                                                // Success! Both ACKs received
                                                let elapsed = start_time.elapsed();
                                                println!("✓ Saved sample (header+data) for slot {} in {:.3}s", slot, elapsed.as_secs_f64());

                                                // Clear name_edited flag and loading state on main thread
                                                let ui_weak_clone = ui_weak.clone();
                                                slint::invoke_from_event_loop(move || {
                                                    if let Some(ui) = ui_weak_clone.upgrade() {
                                                        ui.invoke_clear_name_edited_flag(slot);
                                                        ui.set_loading_slot(-1);
                                                    }
                                                }).ok();
                                            }
                                            Err(e) => {
                                                println!("✗ Failed to receive ACK after data: {}", e);
                                                let ui_weak_clone = ui_weak.clone();
                                                slint::invoke_from_event_loop(move || {
                                                    if let Some(ui) = ui_weak_clone.upgrade() {
                                                        ui.set_loading_slot(-1);
                                                    }
                                                }).ok();
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        println!("✗ Failed to send data: {}", e);
                                        let ui_weak_clone = ui_weak.clone();
                                        slint::invoke_from_event_loop(move || {
                                            if let Some(ui) = ui_weak_clone.upgrade() {
                                                ui.set_loading_slot(-1);
                                            }
                                        }).ok();
                                    }
                                }
                            }
                            Err(e) => {
                                println!("✗ Failed to receive ACK after header: {}", e);
                                let ui_weak_clone = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_clone.upgrade() {
                                        ui.set_loading_slot(-1);
                                    }
                                }).ok();
                            }
                        }
                    }
                    Err(e) => {
                        println!("✗ Failed to send header: {}", e);
                        let ui_weak_clone = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_clone.upgrade() {
                                ui.set_loading_slot(-1);
                            }
                        }).ok();
                    }
                }
            } else {
                println!("ERROR: Device not connected");
                let ui_weak_clone = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_clone.upgrade() {
                        ui.set_loading_slot(-1);
                    }
                }).ok();
            }
        });
    });
}
