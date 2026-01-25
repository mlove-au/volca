# Volca Sample 2 Manager

A native macOS application for managing samples on the KORG Volca Sample 2 via USB MIDI.

## Project Structure

This is a Cargo workspace with two crates:

### volsa2-core (Library)
Platform-independent protocol and audio processing library derived from [volsa2-cli](https://github.com/00nktk/volsa2).

**Contains:**
- SysEx protocol implementation for Volca Sample 2
- 7-bit MIDI-safe data encoding
- WAV file conversion and resampling (31.25kHz)
- Helper utilities

**License:** MIT (see volsa2-core/LICENSE)
**Attribution:** Original work by 00nktk

### volca-manager (Application)
macOS GUI application built with Slint that provides:

- View all 200 sample slots
- Upload WAV files to device
- Download samples from device
- Delete samples
- Audio preview
- Batch operations (folder upload, backup all)
- Fast USB MIDI transfers (no audio cable required)

## Building

Requires:
- Rust 1.64.0 or higher
- macOS (uses CoreMIDI via midir)

```bash
cargo build --release
```

## Running

```bash
cargo run --package volca-manager
```

Or after building:

```bash
./target/release/volca-manager
```

## Development

The project uses:
- **Slint** for native-feeling GUI
- **midir** for cross-platform MIDI communication
- **rodio** for audio playback
- **hound** + **rubato** for audio processing

## License

- `volsa2-core`: MIT License (derived from volsa2-cli by 00nktk)
- `volca-manager`: MIT License

## References

- Original volsa2-cli: https://github.com/00nktk/volsa2
- KORG Volca Sample 2: https://www.korg.com/us/products/dj/volca_sample2/
- SYRO Protocol: https://korginc.github.io/volcasample/documentation.html
