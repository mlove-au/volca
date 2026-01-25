use slint::{VecModel, SharedString};
use std::rc::Rc;

// SampleInfo will be provided from the parent module that includes Slint types

pub struct AppState {
    pub samples: Rc<VecModel<SampleInfo>>,
    pub connected: bool,
}

impl AppState {
    pub fn new() -> Self {
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
        }
    }

    /// Load mock data for testing UI
    pub fn load_mock_data(&mut self) {
        let mock_samples = vec![
            (0, "Kick", 31250),
            (1, "Snare", 15625),
            (2, "HiHat", 7812),
            (4, "Clap", 12500),
            (6, "Perc", 10000),
            (7, "Bass", 62500),
            (10, "Synth", 93750),
            (12, "FX", 20000),
            (13, "Vox", 156250),
            (15, "Loop", 125000),
            (16, "Drum", 31250),
            (25, "Tom", 25000),
            (50, "Crash", 50000),
            (99, "Ambient", 200000),
            (100, "Lead", 80000),
            (150, "Pad", 180000),
            (199, "Noise", 15000),
        ];

        for (slot, name, length) in mock_samples {
            self.samples.set_row_data(slot as usize, SampleInfo {
                slot,
                name: SharedString::from(name),
                length,
                speed: 16384,  // Default speed
                has_sample: true,
                name_edited: false,
            });
        }
    }

    pub fn update_sample(&mut self, slot: i32, info: SampleInfo) {
        if slot >= 0 && slot < 200 {
            self.samples.set_row_data(slot as usize, info);
        }
    }

    pub fn get_sample(&self, slot: i32) -> Option<SampleInfo> {
        if slot >= 0 && slot < 200 {
            Some(self.samples.row_data(slot as usize)?)
        } else {
            None
        }
    }
}
