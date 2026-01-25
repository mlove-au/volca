slint::include_modules!();

mod midi;

use midi::MidiDevice;
use slint::{Model, VecModel, SharedString};
use std::rc::Rc;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Generate SVG path commands for waveform display (envelope style)
/// Uses viewbox coordinates: x in [0, 1000], y in [0, 200] where y=100 is center
/// Creates a filled envelope showing min/max values at each point
fn generate_waveform_path(samples: &[i16], num_points: usize) -> String {
    if samples.is_empty() {
        return String::from("M 0 100 L 1000 100 L 1000 100 L 0 100 Z");
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
        // x: 0 to 1000 (full width, guaranteed)
        // y: 0 (top, +1.0) to 200 (bottom, -1.0), center at 100
        let x = t * 1000.0;
        let y_max = 100.0 - (max_val as f64 / i16::MAX as f64) * 98.0;
        let y_min = 100.0 - (min_val as f64 / i16::MAX as f64) * 98.0;

        upper_points.push((x, y_max));
        lower_points.push((x, y_min));
    }

    // Build path: upper envelope forward, then lower envelope backward
    let mut path = String::with_capacity(num_points * 40);

    // Start at x=0 with first point
    path.push_str(&format!("M 0 {:.1}", upper_points.first().map(|p| p.1).unwrap_or(100.0)));

    // Draw upper envelope
    for &(x, y) in upper_points.iter() {
        path.push_str(&format!(" L {:.1} {:.1}", x, y));
    }

    // Ensure we reach x=1000
    if let Some(&(_, y)) = upper_points.last() {
        path.push_str(&format!(" L 1000 {:.1}", y));
    }

    // Draw lower envelope in reverse
    if let Some(&(_, y)) = lower_points.last() {
        path.push_str(&format!(" L 1000 {:.1}", y));
    }
    for &(x, y) in lower_points.iter().rev() {
        path.push_str(&format!(" L {:.1} {:.1}", x, y));
    }

    // Close back to start
    path.push_str(&format!(" L 0 {:.1}", lower_points.first().map(|p| p.1).unwrap_or(100.0)));
    path.push_str(" Z");

    path
}

pub struct AppState {
    pub samples: Rc<VecModel<SampleInfo>>,
    pub connected: bool,
    pub device: Rc<RefCell<Option<MidiDevice>>>,
    pub firmware_version: String,
    pub logs: Rc<VecModel<SharedString>>,
    pub ui: slint::Weak<AppWindow>,
    pub downloaded_samples: Rc<RefCell<HashMap<u8, (Vec<i16>, u16)>>>,  // (audio_data, speed)
    pub stop_sender: Rc<RefCell<Option<Sender<()>>>>,  // Channel to signal stop playback
    pub is_playing: Arc<AtomicBool>,  // Thread-safe playback state
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
                has_sample: false,
            });
        }

        Self {
            samples,
            connected: false,
            device: Rc::new(RefCell::new(None)),
            firmware_version: String::new(),
            logs: Rc::new(VecModel::default()),
            ui,
            downloaded_samples: Rc::new(RefCell::new(HashMap::new())),
            stop_sender: Rc::new(RefCell::new(None)),
            is_playing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Scan all 200 sample slots from the device
    pub fn scan_all_samples(&self) {
        self.log("Starting scan of all 200 sample slots...".to_string());

        if let Some(ui) = self.ui.upgrade() {
            ui.set_scanning(true);
        }

        let mut device_opt = self.device.borrow_mut();
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
                            has_sample,
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
                            has_sample: false,
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
            *app_state.device.borrow_mut() = Some(device);
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

    // Timer to sync playback state from thread to UI
    let is_playing_timer = app_state.is_playing.clone();
    let ui_weak_timer = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(100),
        move || {
            if let Some(ui) = ui_weak_timer.upgrade() {
                let playing = is_playing_timer.load(Ordering::SeqCst);
                if ui.get_is_playing() != playing {
                    ui.set_is_playing(playing);
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
            *state_connect.device.borrow_mut() = None;
            ui.set_connected(false);
            state_connect.log("Disconnected.".to_string());
        } else {
            // Connect
            state_connect.log("Attempting to connect to Volca Sample 2...".to_string());
            match MidiDevice::connect() {
                Ok(device) => {
                    let firmware = device.firmware_version.clone();
                    *state_connect.device.borrow_mut() = Some(device);
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

    // Download callback - fetch and display sample info
    let state_download = state.clone();
    ui.on_download_clicked(move |slot| {
        use std::time::Instant;

        // Check if sample exists in UI state
        let _sample = match state_download.get_sample(slot) {
            Some(s) if s.has_sample => s,
            _ => {
                state_download.log(format!("Slot {} is empty, nothing to download", slot));
                return;
            }
        };

        state_download.log(format!("=== Fetching Sample Info from Slot {} ===", slot));
        state_download.log(format!("Requesting sample header from device..."));

        // Check device connection
        let mut device_guard = state_download.device.borrow_mut();
        let device = match device_guard.as_mut() {
            Some(d) => d,
            None => {
                state_download.log("ERROR: Device not connected".to_string());
                return;
            }
        };

        // Start timing
        let start_time = Instant::now();

        // Fetch sample header
        state_download.log(format!("Sending SampleHeaderDumpRequest (0x1E) for slot {}...", slot));

        match device.get_sample_info(slot as u8) {
            Ok(header) => {
                let elapsed = start_time.elapsed();

                state_download.log(format!("✓ Header received successfully in {:.3}s", elapsed.as_secs_f64()));
                state_download.log(format!(""));
                state_download.log(format!("--- Sample Metadata ---"));
                state_download.log(format!("  Slot:     {}", header.sample_no));
                state_download.log(format!("  Name:     \"{}\"", header.name));
                state_download.log(format!("  Length:   {} samples", header.length));

                // Calculate duration (sample rate = 31.25 kHz)
                let duration_secs = header.length as f64 / 31250.0;
                state_download.log(format!("  Duration: {:.3}s", duration_secs));

                state_download.log(format!("  Level:    {}", header.level));
                state_download.log(format!("  Speed:    {}", header.speed));

                // Calculate file sizes
                let audio_bytes = header.length as usize * 2; // 16-bit samples
                let wav_header_size = 44; // Standard WAV header
                let total_wav_size = audio_bytes + wav_header_size;

                state_download.log(format!(""));
                state_download.log(format!("--- File Information ---"));
                state_download.log(format!("  Audio data:      {} bytes ({:.2} KB)",
                    audio_bytes, audio_bytes as f64 / 1024.0));
                state_download.log(format!("  WAV file size:   {} bytes ({:.2} KB)",
                    total_wav_size, total_wav_size as f64 / 1024.0));
                state_download.log(format!("  Sample rate:     31,250 Hz (Volca Sample 2)"));
                state_download.log(format!("  Bit depth:       16-bit"));
                state_download.log(format!("  Channels:        1 (mono)"));

                // Calculate MIDI transfer overhead
                let midi_encoded_size = (audio_bytes * 8) / 7 +
                    if (audio_bytes * 8) % 7 != 0 { 1 } else { 0 };
                let estimated_transfer_time = midi_encoded_size as f64 / (31250.0 / 8.0);

                state_download.log(format!(""));
                state_download.log(format!("--- MIDI Transfer Information ---"));
                state_download.log(format!("  Header transfer:  {:.3}s", elapsed.as_secs_f64()));
                state_download.log(format!("  Header size:      ~39 bytes"));
                state_download.log(format!("  Data size (MIDI): ~{} bytes (7-bit encoded)", midi_encoded_size));
                state_download.log(format!("  Est. full xfer:   ~{:.1}s", estimated_transfer_time));

                state_download.log(format!(""));
                state_download.log(format!("=== Sample Info Complete ==="));

                // Now download the actual sample data
                state_download.log(format!(""));
                state_download.log(format!("=== Downloading Sample Data ==="));
                state_download.log(format!("Sending SampleDataDumpRequest (0x1F) for slot {}...", slot));
                state_download.log(format!("This may take up to {:.1}s for large samples...", estimated_transfer_time));

                let data_start_time = Instant::now();
                match device.get_sample_data(slot as u8, header.length as usize) {
                    Ok(sample_data) => {
                        let data_elapsed = data_start_time.elapsed();
                        state_download.log(format!("✓ Sample data received in {:.3}s", data_elapsed.as_secs_f64()));
                        state_download.log(format!("  Received {} audio samples ({:.2} KB)",
                            sample_data.data.len(),
                            (sample_data.data.len() * 2) as f64 / 1024.0));

                        // Store in memory with speed metadata for correct playback
                        state_download.downloaded_samples.borrow_mut().insert(slot as u8, (sample_data.data, header.speed));
                        state_download.log(format!("✓ Sample stored in memory (slot {}, speed {})", slot, header.speed));

                        state_download.log(format!(""));
                        state_download.log(format!("=== Download Complete ==="));
                    }
                    Err(e) => {
                        let data_elapsed = data_start_time.elapsed();
                        state_download.log(format!("✗ ERROR: Failed to download sample data after {:.3}s",
                            data_elapsed.as_secs_f64()));
                        state_download.log(format!("  Error: {}", e));
                    }
                }
            }
            Err(e) => {
                let elapsed = start_time.elapsed();
                state_download.log(format!("✗ ERROR: Failed to fetch sample info after {:.3}s",
                    elapsed.as_secs_f64()));
                state_download.log(format!("  Error: {}", e));

                // Provide helpful context
                if e.to_string().contains("timeout") {
                    state_download.log(format!("  Tip: Device may be slow to respond or slot {} may be empty", slot));
                }
            }
        }
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
                // Check if we have the sample data in memory
                if let Some((audio_data, speed)) = state_play.downloaded_samples.borrow().get(&(slot as u8)) {
                    let sample_count = audio_data.len();
                    let effective_rate = (31250.0 * (*speed as f64 / 16384.0)) as u32;
                    state_play.log(format!("Playing {} ({} samples at {} Hz)", sample.name, sample_count, effective_rate));

                    // Create stop channel
                    let (tx, rx) = mpsc::channel();
                    *state_play.stop_sender.borrow_mut() = Some(tx);

                    // Set playing state
                    state_play.is_playing.store(true, Ordering::SeqCst);
                    ui.set_is_playing(true);

                    // Play the audio in a background thread
                    let audio_clone = audio_data.clone();
                    let speed_clone = *speed;
                    let is_playing_flag = state_play.is_playing.clone();
                    std::thread::spawn(move || {
                        let completed = play_audio_sample(&audio_clone, speed_clone, rx);
                        println!("Playback {}", if completed { "complete" } else { "stopped" });
                        is_playing_flag.store(false, Ordering::SeqCst);
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
        if let Some(sender) = state_stop.stop_sender.borrow_mut().take() {
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
            if sample.has_sample {
                ui.set_selected_sample_name(sample.name.clone());
                ui.set_selected_sample_length(sample.length as i32);

                // Check if we have downloaded audio data for this slot
                if let Some((audio_data, _speed)) = state_select.downloaded_samples.borrow().get(&(slot as u8)) {
                    state_select.log(format!("Selected slot {}: {} ({} samples)", slot, sample.name, audio_data.len()));

                    // Generate SVG path for waveform (1000 points for smooth display)
                    let waveform_path = generate_waveform_path(audio_data, 1000);
                    ui.set_waveform_path(SharedString::from(waveform_path));
                } else {
                    state_select.log(format!("Selected slot {}: {} (not downloaded)", slot, sample.name));
                    // Clear waveform path
                    ui.set_waveform_path(SharedString::from(""));
                }
            } else {
                ui.set_selected_sample_name(slint::SharedString::from("Empty slot"));
                ui.set_selected_sample_length(0);
                // Clear waveform path
                ui.set_waveform_path(SharedString::from(""));
            }
        }

        // Reset zoom and scroll when selecting a new sample
        ui.set_waveform_zoom(1.0);
        ui.set_waveform_scroll(0.0);
    });

    // Waveform zoom callbacks
    let ui_weak_zoom_in = ui_weak.clone();
    ui.on_waveform_zoom_in(move || {
        let ui = ui_weak_zoom_in.unwrap();
        let current_zoom = ui.get_waveform_zoom();
        let new_zoom = (current_zoom * 1.5).min(10.0); // Max 10x zoom
        ui.set_waveform_zoom(new_zoom);
        println!("Zoom in: {}%", (new_zoom * 100.0) as i32);
    });

    let ui_weak_zoom_out = ui_weak.clone();
    ui.on_waveform_zoom_out(move || {
        let ui = ui_weak_zoom_out.unwrap();
        let current_zoom = ui.get_waveform_zoom();
        let new_zoom = (current_zoom / 1.5).max(1.0); // Min 1x zoom
        ui.set_waveform_zoom(new_zoom);
        // Reset scroll if we can see the whole waveform
        if new_zoom <= 1.0 {
            ui.set_waveform_scroll(0.0);
        }
        println!("Zoom out: {}%", (new_zoom * 100.0) as i32);
    });

    let ui_weak_zoom_reset = ui_weak.clone();
    ui.on_waveform_zoom_reset(move || {
        let ui = ui_weak_zoom_reset.unwrap();
        ui.set_waveform_zoom(1.0);
        ui.set_waveform_scroll(0.0);
        println!("Zoom reset");
    });
}
