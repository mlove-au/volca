# volsa2-core

This library contains protocol and audio processing code derived from [volsa2-cli](https://github.com/00nktk/volsa2) by 00nktk.

## Origin

The original `volsa2-cli` is a command-line tool for managing samples on the KORG Volca Sample 2 over ALSA (Linux). This library extracts the platform-independent components:

- **Protocol layer** (`proto/`) - SysEx message encoding/decoding for Volca Sample 2
- **7-bit encoding** (`seven_bit.rs`) - MIDI-safe data encoding
- **Audio processing** (`audio.rs`) - WAV conversion and resampling to 31.25kHz
- **Utilities** (`util.rs`) - Helper functions

## Modifications

This fork makes the following changes:

1. **Library structure**: Reorganized as a reusable library crate (original was binary-only)
2. **Platform independence**: Removed ALSA-specific code to enable cross-platform usage
3. **API exposure**: Public API for integration with GUI applications

## Usage

```rust
use volsa2_core::proto::{SampleHeader, SampleData};
use volsa2_core::seven_bit::U7;

// Create a sample header
let header = SampleHeader {
    sample_no: 0,
    name: "Kick".to_string(),
    length: 44100,
    level: 100,
    speed: 0,
};

// Encode for transmission
let encoded = header.encode();
```

## License

MIT License - see LICENSE file for full text.

## Attribution

Original work: Copyright (c) 2023 00nktk (https://github.com/00nktk/volsa2)
Modifications: Copyright (c) 2026 volca-manager contributors

## Links

- Original project: https://github.com/00nktk/volsa2
- Original crate: https://crates.io/crates/volsa2-cli
- KORG Volca Sample 2: https://www.korg.com/us/products/dj/volca_sample2/
