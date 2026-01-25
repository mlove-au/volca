slint::include_modules!();

mod midi;

use midi::MidiDevice;
use slint::{Model, VecModel, SharedString};
use std::rc::Rc;
use std::cell::RefCell;
use std::collections::HashMap;

pub struct AppState {
    pub samples: Rc<VecModel<SampleInfo>>,
    pub connected: bool,
    pub device: Rc<RefCell<Option<MidiDevice>>>,
    pub firmware_version: String,
    pub logs: Rc<VecModel<SharedString>>,
    pub ui: slint::Weak<AppWindow>,
    pub downloaded_samples: Rc<RefCell<HashMap<u8, (Vec<i16>, u16)>>>,  // (audio_data, speed)
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
        }
    }
}

/// Play audio samples using rodio with speed-adjusted sample rate
fn play_audio_sample(samples: &[i16], speed: u16) -> Result<(), Box<dyn std::error::Error>> {
    use rodio::{OutputStream, Sink};
    use rodio::buffer::SamplesBuffer;

    println!("DEBUG: play_audio_sample called with {} samples, speed {}", samples.len(), speed);

    // Create output stream
    let (_stream, stream_handle) = OutputStream::try_default()?;
    let sink = Sink::try_new(&stream_handle)?;

    // Volca Sample 2 base specs: 31,250 Hz, mono, 16-bit
    // Speed is a fixed-point value where 16384 = 1.0x
    const BASE_RATE: u32 = 31250;
    const DEFAULT_SPEED: u16 = 16384;
    const CHANNELS: u16 = 1;

    // Calculate effective sample rate based on speed metadata
    let sample_rate = (BASE_RATE as f64 * (speed as f64 / DEFAULT_SPEED as f64)) as u32;

    println!("DEBUG: Creating SamplesBuffer with {} samples at {} Hz (speed {})", samples.len(), sample_rate, speed);
    let duration_secs = samples.len() as f64 / sample_rate as f64;
    println!("DEBUG: Expected playback duration: {:.3}s", duration_secs);

    // Create audio buffer from i16 samples
    let buffer = SamplesBuffer::new(CHANNELS, sample_rate, samples.to_vec());

    // Add to sink and play
    sink.append(buffer);
    println!("DEBUG: Starting playback...");
    sink.sleep_until_end();
    println!("DEBUG: Playback complete");

    Ok(())
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

    // Play callback
    let state_play = state.clone();
    ui.on_play_clicked(move |slot| {
        if let Some(sample) = state_play.get_sample(slot) {
            if sample.has_sample {
                state_play.log(format!("Play slot {}: {}", slot, sample.name));

                // Check if we have the sample data in memory
                if let Some((audio_data, speed)) = state_play.downloaded_samples.borrow().get(&(slot as u8)) {
                    let sample_count = audio_data.len();
                    // Calculate effective sample rate for display
                    let effective_rate = (31250.0 * (*speed as f64 / 16384.0)) as u32;
                    state_play.log(format!("Playing {} samples at {} Hz (speed {})...", sample_count, effective_rate, speed));

                    // Play the audio in a background thread
                    let audio_clone = audio_data.clone();
                    let speed_clone = *speed;
                    std::thread::spawn(move || {
                        match play_audio_sample(&audio_clone, speed_clone) {
                            Ok(()) => {
                                println!("✓ Playback complete");
                            }
                            Err(e) => {
                                eprintln!("✗ Playback error: {}", e);
                            }
                        }
                    });
                } else {
                    state_play.log(format!("Sample not downloaded yet. Click the download button first."));
                }
            }
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
}
