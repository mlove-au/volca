// Quick test to list MIDI devices
use midir::{MidiInput, MidiOutput};

fn main() {
    println!("=== MIDI Input Ports ===");
    match MidiInput::new("test") {
        Ok(midi_in) => {
            let ports = midi_in.ports();
            if ports.is_empty() {
                println!("  No input ports found");
            } else {
                for (i, port) in ports.iter().enumerate() {
                    println!("  [{}] {}", i, midi_in.port_name(port).unwrap_or_default());
                }
            }
        }
        Err(e) => println!("Error: {}", e),
    }

    println!("\n=== MIDI Output Ports ===");
    match MidiOutput::new("test") {
        Ok(midi_out) => {
            let ports = midi_out.ports();
            if ports.is_empty() {
                println!("  No output ports found");
            } else {
                for (i, port) in ports.iter().enumerate() {
                    println!("  [{}] {}", i, midi_out.port_name(port).unwrap_or_default());
                }
            }
        }
        Err(e) => println!("Error: {}", e),
    }
}
