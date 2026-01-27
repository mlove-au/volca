slint::include_modules!();

// Local modules
mod audio;
mod midi;
mod waveform;

// Standard library
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// External crates
use slint::{Model, SharedString, VecModel};
use tracing::{debug, error, info, warn};

// Local imports
use audio::{load_wav_file, play_audio_sample, resample_audio, OriginalSample};
use midi::MidiDevice;
use waveform::generate_waveform_path;

// Constants
const TOTAL_SLOTS: usize = 200;
const MAX_NAME_LENGTH: usize = 24;
const WAVEFORM_POINTS: usize = 1000;
const VOLCA_SAMPLE_RATE: u32 = 31250;

pub struct AppState {
    pub samples: Rc<VecModel<SampleInfo>>,
    pub connected: bool,
    pub device: Arc<Mutex<Option<MidiDevice>>>,
    pub firmware_version: String,
    pub logs: Rc<VecModel<SharedString>>,
    pub ui: slint::Weak<AppWindow>,
    pub downloaded_samples: Arc<Mutex<HashMap<u8, OriginalSample>>>,
    pub stop_sender: Arc<Mutex<Option<Sender<()>>>>,
    pub is_playing: Arc<AtomicBool>,
    pub playback_info: Arc<Mutex<Option<(Instant, f64)>>>,
}

impl AppState {
    pub fn log(&self, message: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S");
        let log_msg = format!("[{}] {}", timestamp, message);

        self.logs.push(SharedString::from(log_msg.clone()));

        // Update UI with joined text
        self.update_log_text_ui();

        // Also log via tracing
        info!("{}", message);
    }

    fn update_log_text_ui(&self) {
        if let Some(ui) = self.ui.upgrade() {
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

        // Initialize with empty slots
        for i in 0..TOTAL_SLOTS {
            samples.push(SampleInfo {
                slot: i as i32,
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

    /// Scan all sample slots from the device
    pub fn scan_all_samples(&self) {
        self.log("Starting scan of all sample slots...".to_string());

        if let Some(ui) = self.ui.upgrade() {
            ui.set_scanning(true);
        }

        let mut device_opt = self.device.lock().unwrap();
        if let Some(device) = device_opt.as_mut() {
            let mut samples_found = 0;

            for slot in 0..TOTAL_SLOTS {
                // Log progress every 20 slots
                if slot % 20 == 0 {
                    self.log(format!(
                        "Scanning slots {}-{}...",
                        slot,
                        (slot + 19).min(TOTAL_SLOTS - 1)
                    ));
                }

                match device.get_sample_info(slot as u8) {
                    Ok(header) => {
                        let has_sample = header.length > 0;
                        if has_sample {
                            samples_found += 1;
                        }

                        self.samples.set_row_data(
                            slot,
                            SampleInfo {
                                slot: slot as i32,
                                name: SharedString::from(if has_sample {
                                    header.name.as_str()
                                } else {
                                    "(empty)"
                                }),
                                length: header.length as i32,
                                speed: header.speed as i32,
                                has_sample,
                                name_edited: false,
                            },
                        );

                        // Update UI periodically
                        if let Some(ui) = self.ui.upgrade() {
                            ui.set_samples(self.samples.clone().into());
                        }
                    }
                    Err(e) => {
                        warn!("Error reading slot {}: {}", slot, e);
                        self.samples.set_row_data(
                            slot,
                            SampleInfo {
                                slot: slot as i32,
                                name: SharedString::from("(error)"),
                                length: 0,
                                speed: 0,
                                has_sample: false,
                                name_edited: false,
                            },
                        );
                    }
                }
            }

            self.log(format!(
                "Scan complete! Found {} samples in {} slots.",
                samples_found, TOTAL_SLOTS
            ));
        } else {
            self.log("Error: No device connected".to_string());
        }

        if let Some(ui) = self.ui.upgrade() {
            ui.set_scanning(false);
        }
    }

    pub fn update_sample(&mut self, slot: i32, info: SampleInfo) {
        if slot >= 0 && (slot as usize) < TOTAL_SLOTS {
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

/// Helper to clear loading slot indicator from any thread
fn clear_loading_slot(ui_weak: &slint::Weak<AppWindow>) {
    let ui_weak_clone = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak_clone.upgrade() {
            ui.set_loading_slot(-1);
        }
    })
    .ok();
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
            app_state.log(format!(
                "Auto-connect failed: {}. Click Connect to retry.",
                e
            ));
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
        std::time::Duration::from_millis(50),
        move || {
            if let Some(ui) = ui_weak_timer.upgrade() {
                let playing = is_playing_timer.load(Ordering::SeqCst);
                if ui.get_is_playing() != playing {
                    ui.set_is_playing(playing);
                    if !playing {
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

/// Update UI after downloading a sample from the device
fn update_ui_after_download(
    ui_weak: &slint::Weak<AppWindow>,
    slot: i32,
    waveform_data: Vec<i16>,
    header_speed: u16,
    update_sample_info: Option<(String, u32)>,
) {
    let ui_weak_clone = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak_clone.upgrade() {
            // Optionally update SampleInfo with fresh data from device
            if let Some((name, length)) = update_sample_info {
                ui.invoke_update_sample_from_device(
                    slot,
                    SharedString::from(name),
                    length as i32,
                    header_speed as i32,
                );
            }

            // Generate and display waveform
            let width = ui.get_waveform_display_width();
            let height = ui.get_waveform_display_height();
            let waveform_path =
                generate_waveform_path(&waveform_data, WAVEFORM_POINTS, width as f64, height as f64);
            ui.set_waveform_path(SharedString::from(waveform_path));

            // Clear loading state
            ui.set_loading_slot(-1);
        }
    })
    .ok();
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
            state_connect.log("Disconnecting from device...".to_string());
            *state_connect.device.lock().unwrap() = None;
            ui.set_connected(false);
            state_connect.log("Disconnected.".to_string());
        } else {
            state_connect.log("Attempting to connect to Volca Sample 2...".to_string());
            match MidiDevice::connect() {
                Ok(device) => {
                    let firmware = device.firmware_version.clone();
                    *state_connect.device.lock().unwrap() = Some(device);
                    ui.set_connected(true);
                    state_connect.log(format!("Successfully connected! Firmware: {}", firmware));
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
    let state_upload = state.clone();
    ui.on_upload_clicked(move |slot| {
        state_upload.log(format!("Upload to slot {} clicked", slot));
        // TODO: Implement upload
    });

    // Download callback - refresh sample from device with loading state
    let state_download = state.clone();
    let ui_weak_download = ui_weak.clone();
    ui.on_download_clicked(move |slot| {
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

        ui.set_loading_slot(slot);
        state_download.log(format!("Refreshing slot {}: {}...", slot, sample.name));

        let device_ref = state_download.device.clone();
        let downloaded_ref = state_download.downloaded_samples.clone();
        let ui_weak = state_download.ui.clone();

        std::thread::spawn(move || {
            let start_time = Instant::now();
            let mut device_guard = device_ref.lock().unwrap();

            let Some(device) = device_guard.as_mut() else {
                error!("Device not connected");
                clear_loading_slot(&ui_weak);
                return;
            };

            let header = match device.get_sample_info(slot as u8) {
                Ok(h) => h,
                Err(e) => {
                    error!("Failed to get sample info: {}", e);
                    clear_loading_slot(&ui_weak);
                    return;
                }
            };

            let sample_data = match device.get_sample_data(slot as u8) {
                Ok(d) => d,
                Err(e) => {
                    error!("Refresh failed: {}", e);
                    clear_loading_slot(&ui_weak);
                    return;
                }
            };

            let elapsed = start_time.elapsed();
            let audio_data_clone = sample_data.data.clone();

            downloaded_ref.lock().unwrap().insert(
                slot as u8,
                OriginalSample {
                    samples: audio_data_clone.clone(),
                    original_sample_rate: VOLCA_SAMPLE_RATE,
                    current_speed: header.speed,
                },
            );

            debug!("Refreshed in {:.3}s", elapsed.as_secs_f64());

            update_ui_after_download(
                &ui_weak,
                slot,
                audio_data_clone,
                header.speed,
                Some((header.name.clone(), header.length)),
            );
        });
    });

    // Play callback
    let state_play = state.clone();
    let ui_weak_play = ui_weak.clone();
    ui.on_play_clicked(move || {
        let ui = ui_weak_play.unwrap();
        let slot = ui.get_selected_slot();

        if slot < 0 {
            state_play.log("No sample selected".to_string());
            return;
        }

        let Some(sample) = state_play.get_sample(slot) else {
            return;
        };

        if !sample.has_sample {
            return;
        }

        let maybe_audio_data = state_play
            .downloaded_samples
            .lock()
            .unwrap()
            .get(&(slot as u8))
            .map(|orig| (orig.samples.clone(), orig.current_speed));

        let Some((audio_data, speed)) = maybe_audio_data else {
            state_play.log("Sample not downloaded yet. Click the refresh button first.".to_string());
            return;
        };

        let sample_count = audio_data.len();
        let effective_rate = (31250.0 * (speed as f64 / 16384.0)) as u32;
        let duration_secs = sample_count as f64 / effective_rate as f64;
        state_play.log(format!("Playing {} ({:.1}s)", sample.name, duration_secs));

        let (tx, rx) = mpsc::channel();
        *state_play.stop_sender.lock().unwrap() = Some(tx);

        state_play.is_playing.store(true, Ordering::SeqCst);
        *state_play.playback_info.lock().unwrap() = Some((Instant::now(), duration_secs));
        ui.set_is_playing(true);
        ui.set_playback_position(0.0);

        let is_playing_flag = state_play.is_playing.clone();
        let playback_info_flag = state_play.playback_info.clone();
        std::thread::spawn(move || {
            let completed = play_audio_sample(&audio_data, speed, rx);
            debug!("Playback {}", if completed { "complete" } else { "stopped" });
            is_playing_flag.store(false, Ordering::SeqCst);
            *playback_info_flag.lock().unwrap() = None;
        });
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

    // Debug log toggle callback
    let ui_weak_toggle = ui_weak.clone();
    ui.on_toggle_debug_log(move || {
        let ui = ui_weak_toggle.unwrap();
        let current = ui.get_show_debug_log();
        debug!("Toggling debug log: {} -> {}", current, !current);
        ui.set_show_debug_log(!current);
    });

    // Pad selection callback
    let state_select = state.clone();
    ui.on_pad_selected(move |slot| {
        debug!("Pad selected: slot {}", slot);
        let ui = state_select.ui.upgrade().unwrap();
        ui.set_selected_slot(slot);

        let Some(sample) = state_select.get_sample(slot) else {
            return;
        };

        if !sample.has_sample {
            return;
        }

        ui.set_selected_sample_name(sample.name.clone());
        ui.set_selected_sample_length(sample.length);

        // Stop any active playback when switching samples
        if let Some(sender) = state_select.stop_sender.lock().unwrap().take() {
            let _ = sender.send(());
        }
        state_select.is_playing.store(false, Ordering::SeqCst);
        *state_select.playback_info.lock().unwrap() = None;
        ui.set_is_playing(false);
        ui.set_playback_position(0.0);

        // Check if we have downloaded audio data for this slot
        let maybe_sample_data = state_select
            .downloaded_samples
            .lock()
            .unwrap()
            .get(&(slot as u8))
            .map(|orig| orig.samples.clone());

        if let Some(audio_data) = maybe_sample_data {
            state_select.log(format!(
                "Selected slot {}: {} ({} samples)",
                slot,
                sample.name,
                audio_data.len()
            ));

            let width = ui.get_waveform_display_width();
            let height = ui.get_waveform_display_height();
            let waveform_path =
                generate_waveform_path(&audio_data, WAVEFORM_POINTS, width as f64, height as f64);
            ui.set_waveform_path(SharedString::from(waveform_path));
        } else {
            // Sample not downloaded yet - download it automatically
            state_select.log(format!(
                "Selected slot {}: {} (downloading...)",
                slot, sample.name
            ));
            ui.set_waveform_path(SharedString::from(""));
            ui.set_loading_slot(slot);

            let device_ref = state_select.device.clone();
            let downloaded_ref = state_select.downloaded_samples.clone();
            let ui_weak = state_select.ui.clone();

            std::thread::spawn(move || {
                let start_time = Instant::now();
                let mut device_guard = device_ref.lock().unwrap();

                let Some(device) = device_guard.as_mut() else {
                    error!("Device not connected");
                    clear_loading_slot(&ui_weak);
                    return;
                };

                let header = match device.get_sample_info(slot as u8) {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to get sample info: {}", e);
                        clear_loading_slot(&ui_weak);
                        return;
                    }
                };

                let sample_data = match device.get_sample_data(slot as u8) {
                    Ok(d) => d,
                    Err(e) => {
                        error!("Download failed: {}", e);
                        clear_loading_slot(&ui_weak);
                        return;
                    }
                };

                let elapsed = start_time.elapsed();
                let audio_data_clone = sample_data.data.clone();

                downloaded_ref.lock().unwrap().insert(
                    slot as u8,
                    OriginalSample {
                        samples: audio_data_clone.clone(),
                        original_sample_rate: VOLCA_SAMPLE_RATE,
                        current_speed: header.speed,
                    },
                );

                debug!("Downloaded in {:.3}s", elapsed.as_secs_f64());

                update_ui_after_download(&ui_weak, slot, audio_data_clone, header.speed, None);
            });
        }

        // Reset zoom and scroll when selecting a new sample
        ui.set_waveform_zoom(1.0);
        ui.set_waveform_scroll(0.0);
    });

    // Regenerate waveform when display size changes
    let state_regen = state.clone();
    ui.on_regenerate_waveform(move |width, height| {
        let ui = state_regen.ui.upgrade().unwrap();
        let slot = ui.get_selected_slot();

        let Some(sample) = state_regen.get_sample(slot) else {
            return;
        };

        if !sample.has_sample {
            return;
        }

        let maybe_audio_data = state_regen
            .downloaded_samples
            .lock()
            .unwrap()
            .get(&(slot as u8))
            .map(|orig| orig.samples.clone());

        if let Some(audio_data) = maybe_audio_data {
            let waveform_path =
                generate_waveform_path(&audio_data, WAVEFORM_POINTS, width as f64, height as f64);
            ui.set_waveform_path(SharedString::from(waveform_path));
        }
    });

    // Load WAV callback
    let state_load_wav = state.clone();
    let ui_weak_load_wav = ui_weak.clone();
    ui.on_load_wav_clicked(move || {
        let ui = ui_weak_load_wav.unwrap();
        let slot = ui.get_selected_slot();

        if slot < 0 || slot >= TOTAL_SLOTS as i32 {
            warn!("No slot selected for WAV load");
            return;
        }

        let file = rfd::FileDialog::new()
            .add_filter("WAV Audio", &["wav", "WAV"])
            .set_title("Select WAV file to load")
            .pick_file();

        let Some(path) = file else {
            debug!("No file selected");
            return;
        };

        debug!("Loading WAV: {:?}", path);

        match load_wav_file(&path) {
            Ok((original_sample_rate, samples, speed)) => {
                let original_sample_count = samples.len();
                let duration_secs = original_sample_count as f32 / original_sample_rate as f32;
                let display_rate_khz = 31.25 * speed as f32 / 16384.0;

                debug!(
                    "Loaded {} samples ({:.2}s) at {} Hz (effective {:.2} kHz)",
                    original_sample_count, duration_secs, original_sample_rate, display_rate_khz
                );

                // Extract filename for display
                let filename = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .to_string();

                // Truncate to max name length
                let name = if filename.len() > MAX_NAME_LENGTH {
                    filename.chars().take(MAX_NAME_LENGTH).collect()
                } else {
                    filename
                };

                // Resample to Volca sample rate for waveform display and playback
                match resample_audio(&samples, original_sample_rate, VOLCA_SAMPLE_RATE) {
                    Ok(resampled) => {
                        let resampled_count = resampled.len();

                        // Store original in downloaded_samples (in-memory only)
                        state_load_wav.downloaded_samples.lock().unwrap().insert(
                            slot as u8,
                            OriginalSample {
                                samples: samples.clone(),
                                original_sample_rate,
                                current_speed: speed,
                            },
                        );

                        // Update SampleInfo with new metadata and mark as edited
                        let updated_info = SampleInfo {
                            slot,
                            name: SharedString::from(&name),
                            length: resampled_count as i32,
                            speed: speed as i32,
                            has_sample: true,
                            name_edited: true,
                        };
                        state_load_wav
                            .samples
                            .set_row_data(slot as usize, updated_info);

                        // Regenerate waveform display using resampled data
                        let width = ui.get_waveform_display_width();
                        let height = ui.get_waveform_display_height();
                        let waveform_path = generate_waveform_path(
                            &resampled,
                            WAVEFORM_POINTS,
                            width as f64,
                            height as f64,
                        );

                        // Update UI
                        ui.set_selected_sample_name(SharedString::from(&name));
                        ui.set_selected_sample_length(resampled_count as i32);
                        ui.set_waveform_path(SharedString::from(waveform_path));
                        ui.set_samples(state_load_wav.samples.clone().into());

                        state_load_wav.log(format!(
                            "Loaded WAV: {} ({:.2}s, {:.2} kHz) - not saved to device",
                            name, duration_secs, display_rate_khz
                        ));
                    }
                    Err(e) => {
                        error!("Resampling WAV failed: {}", e);
                        state_load_wav.log(format!("Failed to resample WAV: {}", e));
                    }
                }
            }
            Err(e) => {
                error!("Loading WAV failed: {}", e);
                state_load_wav.log(format!("Failed to load WAV: {}", e));
            }
        }
    });

    // Name edit accepted callback
    let state_name_edit = state.clone();
    ui.on_name_edit_accepted(move |slot, new_name| {
        let new_name_str = new_name.to_string();

        state_name_edit.log(format!(
            "Name edited for slot {}: \"{}\" (not yet saved)",
            slot, new_name_str
        ));

        if let Some(mut sample) = state_name_edit.get_sample(slot) {
            sample.name = SharedString::from(&new_name_str);
            sample.name_edited = true;
            state_name_edit.samples.set_row_data(slot as usize, sample);

            if let Some(ui) = state_name_edit.ui.upgrade() {
                ui.set_samples(state_name_edit.samples.clone().into());
            }
        }
    });

    // Clear name edited flag callback
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

    // Update sample from device callback
    let state_update_sample = state.clone();
    ui.on_update_sample_from_device(move |slot, name, length, speed| {
        let updated_info = SampleInfo {
            slot,
            name,
            length,
            speed,
            has_sample: true,
            name_edited: false,
        };
        state_update_sample
            .samples
            .set_row_data(slot as usize, updated_info);

        if let Some(ui) = state_update_sample.ui.upgrade() {
            ui.set_samples(state_update_sample.samples.clone().into());
        }
    });

    // Save to device callback
    let state_save = state.clone();
    let ui_weak_save = ui_weak.clone();
    ui.on_save_to_device_clicked(move |slot| {
        let sample = match state_save.get_sample(slot) {
            Some(s) if s.has_sample => s,
            _ => {
                state_save.log(format!("Cannot save slot {}: no sample exists", slot));
                return;
            }
        };

        let ui = ui_weak_save.unwrap();
        ui.set_loading_slot(slot);
        state_save.log(format!(
            "Saving sample info for slot {}: \"{}\"...",
            slot, sample.name
        ));

        let device_ref = state_save.device.clone();
        let downloaded_ref = state_save.downloaded_samples.clone();
        let ui_weak = state_save.ui.clone();
        let sample_name = sample.name.to_string();

        std::thread::spawn(move || {
            use std::time::Duration;
            use volsa2_core::proto::{SampleData, SampleHeader, Status};

            let start_time = Instant::now();

            // Get original sample and resample to Volca sample rate for device
            let (audio_data, final_speed) =
                match downloaded_ref.lock().unwrap().get(&(slot as u8)) {
                    Some(orig) => match orig.resample_to(VOLCA_SAMPLE_RATE) {
                        Ok(result) => result,
                        Err(e) => {
                            error!("Resampling for save failed: {}", e);
                            clear_loading_slot(&ui_weak);
                            return;
                        }
                    },
                    None => {
                        error!("Sample not downloaded. Download first before saving.");
                        clear_loading_slot(&ui_weak);
                        return;
                    }
                };

            let mut device_guard = device_ref.lock().unwrap();
            let Some(device) = device_guard.as_mut() else {
                error!("Device not connected");
                clear_loading_slot(&ui_weak);
                return;
            };

            let header = SampleHeader {
                sample_no: slot as u8,
                name: sample_name.clone(),
                length: audio_data.len() as u32,
                level: 65535,
                speed: final_speed,
            };

            let data = SampleData {
                sample_no: slot as u8,
                data: audio_data,
            };

            // Send header
            if let Err(e) = device.send_message(&header) {
                error!("Failed to send header: {}", e);
                clear_loading_slot(&ui_weak);
                return;
            }

            // Wait for ACK after header
            match device.receive_message::<Status>(Duration::from_millis(500)) {
                Ok(status) => {
                    if let Err(e) = status {
                        error!("Device NAK after header: {}", e);
                        clear_loading_slot(&ui_weak);
                        return;
                    }
                }
                Err(e) => {
                    error!("Failed to receive ACK after header: {}", e);
                    clear_loading_slot(&ui_weak);
                    return;
                }
            }

            // Send data
            if let Err(e) = device.send_message(&data) {
                error!("Failed to send data: {}", e);
                clear_loading_slot(&ui_weak);
                return;
            }

            // Wait for ACK after data
            match device.receive_message::<Status>(Duration::from_millis(500)) {
                Ok(status) => {
                    if let Err(e) = status {
                        error!("Device NAK after data: {}", e);
                        clear_loading_slot(&ui_weak);
                        return;
                    }
                }
                Err(e) => {
                    error!("Failed to receive ACK after data: {}", e);
                    clear_loading_slot(&ui_weak);
                    return;
                }
            }

            // Success
            let elapsed = start_time.elapsed();
            info!(
                "Saved sample (header+data) for slot {} in {:.3}s",
                slot,
                elapsed.as_secs_f64()
            );

            let ui_weak_clone = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak_clone.upgrade() {
                    ui.invoke_clear_name_edited_flag(slot);
                    ui.set_loading_slot(-1);
                }
            })
            .ok();
        });
    });
}
