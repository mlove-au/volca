use anyhow::{anyhow, Result};
use midir::{MidiInput, MidiOutput, MidiInputConnection, MidiOutputConnection};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use volsa2_core::proto::{self, Header, Incoming, Outgoing, SearchDeviceRequest, SearchDeviceReply, SampleHeader, SampleHeaderDumpRequest, SampleData, SampleDataDumpRequest};
use volsa2_core::seven_bit::U7;

const DEVICE_NAME: &str = "volca sample";
const CHUNK_SIZE: usize = 256;
const CHUNK_DELAY_MS: u64 = 10;

pub struct MidiDevice {
    _input_conn: MidiInputConnection<()>,
    output_conn: MidiOutputConnection,
    input_buffer: Arc<Mutex<Vec<u8>>>,
    pub channel: U7,
    pub firmware_version: String,
}

impl MidiDevice {
    /// Connect to the Volca Sample 2 device
    pub fn connect() -> Result<Self> {
        println!("Searching for Volca Sample 2...");

        // Create MIDI input and output
        let midi_in = MidiInput::new("Volca Manager Input")
            .map_err(|e| anyhow!("Failed to create MIDI input: {}", e))?;
        let midi_out = MidiOutput::new("Volca Manager Output")
            .map_err(|e| anyhow!("Failed to create MIDI output: {}", e))?;

        // Find the Volca Sample device
        let (in_port, out_port) = Self::find_volca_ports(&midi_in, &midi_out)?;

        println!("Found Volca Sample device!");

        // Set up input buffer for receiving data
        let input_buffer = Arc::new(Mutex::new(Vec::new()));
        let buffer_clone = input_buffer.clone();

        // Connect input with callback
        let input_conn = midi_in.connect(
            &in_port,
            "volca-input",
            move |_timestamp, message, _| {
                // Fast path: filter out 0xF8 clock messages immediately
                // This is an O(n) scan but avoids buffer allocation and mutex contention
                // for messages we'd discard anyway
                let filtered: Vec<u8> = message.iter()
                    .copied()
                    .filter(|&b| b != 0xF8)
                    .collect();

                // Only log and buffer if we have non-clock data
                if !filtered.is_empty() {
                    // Debug: Print raw MIDI bytes received (ignore errors)
                    let _ = std::io::Write::write_fmt(
                        &mut std::io::stderr(),
                        format_args!("DEBUG: Received {} MIDI bytes: {:02X?}\n",
                                    filtered.len(), filtered)
                    );

                    // Accumulate incoming MIDI data (ignore poison errors)
                    if let Ok(mut buffer) = buffer_clone.lock() {
                        buffer.extend_from_slice(&filtered);
                    }
                }
            },
            (),
        ).map_err(|e| anyhow!("Failed to connect MIDI input: {}", e))?;

        // Connect output
        let output_conn = midi_out.connect(&out_port, "volca-output")
            .map_err(|e| anyhow!("Failed to connect MIDI output: {}", e))?;

        let mut device = Self {
            _input_conn: input_conn,
            output_conn,
            input_buffer,
            channel: U7::new(0),
            firmware_version: String::from("unknown"),
        };

        // Verify it's a Volca Sample 2 by sending SearchDevice command
        device.verify_device()?;

        Ok(device)
    }

    /// Find the Volca Sample MIDI ports
    fn find_volca_ports(
        midi_in: &MidiInput,
        midi_out: &MidiOutput,
    ) -> Result<(midir::MidiInputPort, midir::MidiOutputPort)> {
        // Find output port
        let out_ports = midi_out.ports();
        let out_port = out_ports
            .iter()
            .find(|p| {
                midi_out
                    .port_name(p)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(DEVICE_NAME)
            })
            .ok_or_else(|| anyhow!("Volca Sample 2 not found. Make sure it's connected via USB."))?;

        // Find input port
        let in_ports = midi_in.ports();
        let in_port = in_ports
            .iter()
            .find(|p| {
                midi_in
                    .port_name(p)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(DEVICE_NAME)
            })
            .ok_or_else(|| anyhow!("Volca Sample 2 input port not found"))?;

        Ok((in_port.clone(), out_port.clone()))
    }

    /// Verify the device is a Volca Sample 2
    fn verify_device(&mut self) -> Result<()> {
        println!("Verifying device...");

        // Send SearchDevice request with echo byte 42
        let request = SearchDeviceRequest { echo: U7::new(42) };
        self.send_message(&request)?;

        // Receive response
        let response = self.receive_message::<SearchDeviceReply>(Duration::from_secs(2))?;

        println!(
            "Connected to Volca Sample 2! Channel: {}, Firmware: {}",
            response.device_id, response.version
        );

        self.channel = response.device_id;
        self.firmware_version = format!("{}", response.version);

        Ok(())
    }

    /// Send a message to the device
    pub fn send_message<T>(&mut self, msg: &T) -> Result<()>
    where
        T: Outgoing,
    {
        // Encode the message
        let mut buf = Vec::new();
        let header = T::Header::from_channel(self.channel);
        msg.encode(header, &mut buf)?;

        println!("DEBUG: Sending {} bytes: {:02X?}", buf.len(), buf);

        // Send in chunks if needed
        if buf.len() <= CHUNK_SIZE {
            self.output_conn.send(&buf)
                .map_err(|e| anyhow!("Failed to send MIDI: {}", e))?;
        } else {
            // Split into chunks with delays
            for chunk in buf.chunks(CHUNK_SIZE) {
                self.output_conn.send(chunk)
                    .map_err(|e| anyhow!("Failed to send MIDI chunk: {}", e))?;

                // Don't delay after last chunk if it ends with EOX
                if !chunk.ends_with(&[proto::EOX]) {
                    std::thread::sleep(Duration::from_millis(CHUNK_DELAY_MS));
                }
            }
        }

        Ok(())
    }

    /// Receive a message from the device
    pub fn receive_message<T>(&self, timeout: Duration) -> Result<T>
    where
        T: Incoming,
    {
        let start = Instant::now();

        loop {
            {
                let mut buffer = match self.input_buffer.lock() {
                    Ok(b) => b,
                    Err(poisoned) => poisoned.into_inner(), // Recover from poisoned mutex
                };

                // Debug: Show current buffer state
                if !buffer.is_empty() {
                    println!("DEBUG: Buffer contains {} bytes", buffer.len());
                }

                // Check if we have a complete SysEx message (starts with 0xF0, ends with 0xF7)
                if let Some(start_idx) = buffer.iter().position(|&b| b == proto::EST) {
                    if let Some(end_idx) = buffer[start_idx..].iter().position(|&b| b == proto::EOX) {
                        // Discard any bytes before the SysEx start (e.g., MIDI clock 0xF8)
                        if start_idx > 0 {
                            buffer.drain(..start_idx);
                            println!("DEBUG: Discarded {} garbage bytes before SysEx", start_idx);
                        }

                        // Extract the complete message (now starting from index 0)
                        let message_end = end_idx + 1;
                        let mut message: Vec<u8> = buffer.drain(..message_end).collect();

                        // Filter out MIDI real-time messages (0xF9-0xFF) that can appear inside SysEx
                        // Note: 0xF8 (Clock) is already filtered in the callback for performance
                        // These are: 0xF9 (undefined), 0xFA (Start), 0xFB (Continue), 0xFC (Stop),
                        //            0xFE (Active Sensing), 0xFF (System Reset)
                        let original_len = message.len();
                        message.retain(|&b| b < 0xF8 || b == proto::EOX);
                        if message.len() != original_len {
                            println!("DEBUG: Filtered out {} MIDI real-time bytes from SysEx", original_len - message.len());
                        }

                        println!("DEBUG: Extracted complete SysEx message ({} bytes): {:02X?}", message.len(), &message[..message.len().min(50)]);

                        // Parse the message
                        match T::parse(&message) {
                            Ok((_, parsed)) => {
                                println!("DEBUG: Successfully parsed message");
                                // Check if more data is already in the buffer
                                println!("DEBUG: Buffer has {} bytes remaining after parsing", buffer.len());
                                if buffer.len() > 0 {
                                    println!("DEBUG: Remaining buffer starts with: {:02X?}", &buffer[..buffer.len().min(50)]);
                                }
                                return Ok(parsed);
                            }
                            Err(e) => {
                                println!("DEBUG: Parse error: {}", e);
                                println!("DEBUG: Message was: {:02X?}", message);
                                return Err(anyhow!("Failed to parse message: {}", e));
                            }
                        }
                    }
                }
            }

            // Check timeout
            if start.elapsed() > timeout {
                let buffer_len = self.input_buffer.lock()
                    .map(|b| b.len()).unwrap_or(0);
                println!("DEBUG: Timeout after {:?}, buffer has {} bytes", timeout, buffer_len);
                return Err(anyhow!("Receive timeout"));
            }

            // Small delay to avoid busy-waiting
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Fetch sample header information from a specific slot
    pub fn get_sample_info(&mut self, slot: u8) -> Result<SampleHeader> {
        // Send request
        let request = SampleHeaderDumpRequest { sample_no: slot };
        self.send_message(&request)?;

        // Receive response with 5 second timeout
        let header = self.receive_message::<SampleHeader>(Duration::from_secs(5))?;

        Ok(header)
    }

    /// Fetch sample audio data from a specific slot
    /// Large samples come in multiple SysEx messages, so we accumulate until we have all the data
    pub fn get_sample_data(&mut self, slot: u8, expected_length: usize) -> Result<SampleData> {
        // Clear buffer to start fresh
        if let Ok(mut buffer) = self.input_buffer.lock() {
            let cleared = buffer.len();
            buffer.clear();
            if cleared > 0 {
                println!("DEBUG: Cleared {} stale bytes from buffer before request", cleared);
            }
        }

        // Send request
        let request = SampleDataDumpRequest { sample_no: slot };
        self.send_message(&request)?;

        println!("DEBUG: Waiting for sample data (header says {} samples)...", expected_length);
        println!("DEBUG: Will accumulate until 2s of silence (header.length may be inaccurate)");

        // Receive first chunk with long timeout
        let mut accumulated = self.receive_message::<SampleData>(Duration::from_secs(120))?;
        println!("DEBUG: Received first chunk: {} samples", accumulated.data.len());

        // Keep receiving until silence (no data for 2 seconds)
        let mut chunk_num = 2;
        let silence_timeout = Duration::from_secs(2);

        loop {
            match self.receive_message::<SampleData>(silence_timeout) {
                Ok(chunk) => {
                    println!("DEBUG: Chunk {}: +{} samples, total: {}",
                        chunk_num, chunk.data.len(), accumulated.data.len() + chunk.data.len());
                    accumulated.data.extend(chunk.data);
                    chunk_num += 1;
                }
                Err(_) => {
                    // Silence - assume transfer complete
                    let buffer_len = self.input_buffer.lock().map(|b| b.len()).unwrap_or(0);
                    println!("DEBUG: Silence detected. Buffer has {} bytes.", buffer_len);
                    break;
                }
            }
        }

        println!("DEBUG: Sample data complete: {} samples ({:.1}% of header)",
            accumulated.data.len(),
            (accumulated.data.len() as f64 / expected_length as f64) * 100.0);
        Ok(accumulated)
    }
}
