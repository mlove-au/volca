// Test CoreMIDI directly - accumulate SysEx properly
use coremidi::{Client, Destination, Destinations, PacketBuffer, Source, Sources};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    println!("=== CoreMIDI SysEx Accumulation Test ===\n");

    // Find Volca Sample
    let source = find_volca_source().expect("Could not find Volca Sample input");
    let dest = find_volca_dest().expect("Could not find Volca Sample output");

    println!("Found Volca Sample!");

    // Create client
    let client = Client::new("volca-test").expect("Failed to create MIDI client");
    let output_port = client.output_port("volca-out").expect("Failed to create output port");

    // Shared buffer for accumulating SysEx
    let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(512 * 1024)));
    let buffer_clone = buffer.clone();

    // Create input port - accumulate everything
    let input_port = client
        .input_port("volca-in", move |packet_list| {
            let mut buf = buffer_clone.lock().unwrap();
            for packet in packet_list.iter() {
                let data = packet.data();
                // Skip clock messages
                if data.len() == 1 && data[0] == 0xF8 {
                    continue;
                }
                buf.extend_from_slice(data);
            }
        })
        .expect("Failed to create input port");

    input_port.connect_source(&source).expect("Failed to connect to source");

    println!("Sending SampleDataDumpRequest for slot 0...");

    let request: &[u8] = &[0xF0, 0x42, 0x30, 0x00, 0x01, 0x2D, 0x1F, 0x00, 0x00, 0xF7];
    let packets = PacketBuffer::new(0, request);
    output_port.send(&dest, &packets).expect("Failed to send");

    println!("Waiting for data (until 2s of no new data)...\n");

    let mut last_len = 0;
    let mut last_change = Instant::now();

    loop {
        std::thread::sleep(Duration::from_millis(100));

        let current_len = buffer.lock().unwrap().len();

        if current_len != last_len {
            println!("Buffer: {} bytes", current_len);
            last_len = current_len;
            last_change = Instant::now();
        }

        // 2 seconds of silence = done
        if last_change.elapsed() > Duration::from_secs(2) && current_len > 0 {
            break;
        }

        // 30 second absolute timeout
        if last_change.elapsed() > Duration::from_secs(30) {
            println!("Timeout!");
            break;
        }
    }

    // Analyze the buffer
    let buf = buffer.lock().unwrap();
    println!("\n=== Results ===");
    println!("Total bytes received: {}", buf.len());

    // Find SysEx boundaries
    let mut sysex_count = 0;
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0xF0 {
            // Find the end
            if let Some(end_offset) = buf[i..].iter().position(|&b| b == 0xF7) {
                let sysex_len = end_offset + 1;
                sysex_count += 1;
                println!(
                    "SysEx #{}: {} bytes (offset {}), ID={:02X?}",
                    sysex_count,
                    sysex_len,
                    i,
                    &buf[i..i + sysex_len.min(10)]
                );

                // If it's a SampleData message (0x4F), decode sample count
                if sysex_len > 9 && buf[i + 6] == 0x4F {
                    // Data starts at offset 9 (after F0 42 30 00 01 2D 4F XX XX)
                    let data_bytes = sysex_len - 10; // -9 for header, -1 for F7
                    // 7-bit to 8-bit: 8 bytes become 7 bytes
                    let decoded_bytes = data_bytes * 7 / 8;
                    let samples = decoded_bytes / 2; // 16-bit samples
                    println!("  -> SampleData: ~{} samples (~{} KB audio)", samples, samples * 2 / 1024);
                }

                i += sysex_len;
            } else {
                println!("Incomplete SysEx at offset {} (no F7 found)", i);
                break;
            }
        } else {
            i += 1;
        }
    }

    println!("\nTotal SysEx messages: {}", sysex_count);

    // Show what's after the SysEx data
    if i < buf.len() {
        println!("\n=== Data after last SysEx ===");
        println!("Remaining bytes: {}", buf.len() - i);
        println!("First 50 bytes: {:02X?}", &buf[i..buf.len().min(i + 50)]);

        // Check if there are any F0 or F7 markers in the remaining data
        let f0_count = buf[i..].iter().filter(|&&b| b == 0xF0).count();
        let f7_count = buf[i..].iter().filter(|&&b| b == 0xF7).count();
        println!("F0 markers in remaining: {}", f0_count);
        println!("F7 markers in remaining: {}", f7_count);
    }
}

fn find_volca_source() -> Option<Source> {
    for source in Sources {
        if let Some(name) = source.display_name() {
            if name.to_lowercase().contains("volca sample") {
                return Some(source);
            }
        }
    }
    None
}

fn find_volca_dest() -> Option<Destination> {
    for dest in Destinations {
        if let Some(name) = dest.display_name() {
            if name.to_lowercase().contains("volca sample") {
                return Some(dest);
            }
        }
    }
    None
}
