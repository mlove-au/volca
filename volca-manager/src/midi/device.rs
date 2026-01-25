use anyhow::{anyhow, Result};
use coremidi::{Client, Destination, Destinations, InputPort, OutputPort, PacketBuffer, Source, Sources};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use volsa2_core::proto::{self, Header, Incoming, Outgoing, SearchDeviceRequest, SearchDeviceReply, SampleHeader, SampleHeaderDumpRequest, SampleData, SampleDataDumpRequest};
use volsa2_core::seven_bit::U7;

const DEVICE_NAME: &str = "volca sample";
const CHUNK_SIZE: usize = 256;
const CHUNK_DELAY_MS: u64 = 10;

pub struct MidiDevice {
    _client: Client,
    _input_port: InputPort,
    output_port: OutputPort,
    destination: Destination,
    input_buffer: Arc<Mutex<Vec<u8>>>,
    pub channel: U7,
    pub firmware_version: String,
}

impl MidiDevice {
    /// Connect to the Volca Sample 2 device
    pub fn connect() -> Result<Self> {
        // Find Volca Sample source and destination
        let source = Self::find_volca_source()
            .ok_or_else(|| anyhow!("Volca Sample 2 not found. Make sure it's connected via USB."))?;
        let destination = Self::find_volca_destination()
            .ok_or_else(|| anyhow!("Volca Sample 2 output not found."))?;

        // Create CoreMIDI client
        let client = Client::new("Volca Manager")
            .map_err(|e| anyhow!("Failed to create MIDI client: {:?}", e))?;

        // Create output port
        let output_port = client.output_port("volca-out")
            .map_err(|e| anyhow!("Failed to create output port: {:?}", e))?;

        // Set up input buffer for receiving data
        let input_buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(512 * 1024)));
        let buffer_clone = input_buffer.clone();

        // Create input port with callback - accumulate ALL packets
        let input_port = client
            .input_port("volca-in", move |packet_list| {
                if let Ok(mut buf) = buffer_clone.lock() {
                    for packet in packet_list.iter() {
                        let data = packet.data();
                        // Skip clock messages (single byte 0xF8)
                        if data.len() == 1 && data[0] == 0xF8 {
                            continue;
                        }
                        buf.extend_from_slice(data);
                    }
                }
            })
            .map_err(|e| anyhow!("Failed to create input port: {:?}", e))?;

        // Connect to Volca source
        input_port.connect_source(&source)
            .map_err(|e| anyhow!("Failed to connect to source: {:?}", e))?;

        let mut device = Self {
            _client: client,
            _input_port: input_port,
            output_port,
            destination,
            input_buffer,
            channel: U7::new(0),
            firmware_version: String::from("unknown"),
        };

        // Verify it's a Volca Sample 2 by sending SearchDevice command
        device.verify_device()?;

        Ok(device)
    }

    fn find_volca_source() -> Option<Source> {
        for source in Sources {
            if let Some(name) = source.display_name() {
                if name.to_lowercase().contains(DEVICE_NAME) {
                    return Some(source);
                }
            }
        }
        None
    }

    fn find_volca_destination() -> Option<Destination> {
        for dest in Destinations {
            if let Some(name) = dest.display_name() {
                if name.to_lowercase().contains(DEVICE_NAME) {
                    return Some(dest);
                }
            }
        }
        None
    }

    /// Verify the device is a Volca Sample 2
    fn verify_device(&mut self) -> Result<()> {
        let request = SearchDeviceRequest { echo: U7::new(42) };
        self.send_message(&request)?;

        let response = self.receive_message::<SearchDeviceReply>(Duration::from_secs(2))?;

        self.channel = response.device_id;
        self.firmware_version = format!("{}", response.version);

        Ok(())
    }

    /// Send a message to the device
    pub fn send_message<T>(&mut self, msg: &T) -> Result<()>
    where
        T: Outgoing,
    {
        let mut buf = Vec::new();
        let header = T::Header::from_channel(self.channel);
        msg.encode(header, &mut buf)?;

        // Send in chunks if needed (with delays for large messages)
        if buf.len() <= CHUNK_SIZE {
            let packets = PacketBuffer::new(0, &buf);
            self.output_port.send(&self.destination, &packets)
                .map_err(|e| anyhow!("Failed to send MIDI: {:?}", e))?;
        } else {
            for chunk in buf.chunks(CHUNK_SIZE) {
                let packets = PacketBuffer::new(0, chunk);
                self.output_port.send(&self.destination, &packets)
                    .map_err(|e| anyhow!("Failed to send MIDI chunk: {:?}", e))?;

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
                    Err(poisoned) => poisoned.into_inner(),
                };

                // Check if we have a complete SysEx message (starts with 0xF0, ends with 0xF7)
                if let Some(start_idx) = buffer.iter().position(|&b| b == proto::EST) {
                    if let Some(end_idx) = buffer[start_idx..].iter().position(|&b| b == proto::EOX) {
                        if start_idx > 0 {
                            buffer.drain(..start_idx);
                        }

                        let message_end = end_idx + 1;
                        let message: Vec<u8> = buffer.drain(..message_end).collect();

                        match T::parse(&message) {
                            Ok((_, parsed)) => {
                                return Ok(parsed);
                            }
                            Err(e) => {
                                return Err(anyhow!("Failed to parse message: {}", e));
                            }
                        }
                    }
                }
            }

            if start.elapsed() > timeout {
                return Err(anyhow!("Receive timeout"));
            }

            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Fetch sample header information from a specific slot
    pub fn get_sample_info(&mut self, slot: u8) -> Result<SampleHeader> {
        let request = SampleHeaderDumpRequest { sample_no: slot };
        self.send_message(&request)?;
        self.receive_message::<SampleHeader>(Duration::from_secs(5))
    }

    /// Fetch sample audio data from a specific slot
    pub fn get_sample_data(&mut self, slot: u8, _expected_length: usize) -> Result<SampleData> {
        // Clear buffer
        if let Ok(mut buffer) = self.input_buffer.lock() {
            buffer.clear();
        }

        // Send request
        let request = SampleDataDumpRequest { sample_no: slot };
        self.send_message(&request)?;

        // Wait for data to arrive and accumulate
        let start = Instant::now();
        let mut last_len = 0;
        let mut last_change = Instant::now();

        // Wait until we have data and then 2 seconds of silence
        loop {
            std::thread::sleep(Duration::from_millis(50));

            let current_len = self.input_buffer.lock().map(|b| b.len()).unwrap_or(0);

            if current_len != last_len {
                last_len = current_len;
                last_change = Instant::now();
            }

            // 2 seconds of silence after receiving data = done
            if last_change.elapsed() > Duration::from_secs(2) && current_len > 0 {
                break;
            }

            // 120 second absolute timeout
            if start.elapsed() > Duration::from_secs(120) {
                return Err(anyhow!("Timeout waiting for sample data"));
            }
        }

        // Parse the SysEx message - find F0 at start and LAST F7 in buffer
        let buffer = self.input_buffer.lock().unwrap();

        let start_idx = buffer.iter().position(|&b| b == proto::EST)
            .ok_or_else(|| anyhow!("No SysEx start (F0) found"))?;

        // Find the LAST F7 in the buffer (not the first one - there may be phantom F7s)
        let end_idx = buffer.iter().rposition(|&b| b == proto::EOX)
            .ok_or_else(|| anyhow!("No SysEx end (F7) found"))?;

        if end_idx <= start_idx {
            return Err(anyhow!("Invalid SysEx boundaries"));
        }

        // Extract and clean the message - remove any bytes >= 0x80 except F0/F7
        let raw_message = &buffer[start_idx..=end_idx];
        let mut message: Vec<u8> = Vec::with_capacity(raw_message.len());
        message.push(proto::EST);
        for &byte in &raw_message[1..raw_message.len()-1] {
            if byte < 0x80 {
                message.push(byte);
            }
        }
        message.push(proto::EOX);

        match SampleData::parse(&message) {
            Ok((_, sample_data)) => {
                drop(buffer);
                if let Ok(mut buf) = self.input_buffer.lock() {
                    buf.clear();
                }
                Ok(sample_data)
            }
            Err(e) => {
                Err(anyhow!("Failed to parse sample data: {}", e))
            }
        }
    }
}
