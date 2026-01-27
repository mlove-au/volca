//! Waveform visualization - SVG path generation for audio display.

/// Generate SVG path commands for waveform display (envelope style).
///
/// Coordinates normalized to viewbox dimensions: x in [0, width], y in [0, height].
/// Sample value +1 maps to y=0 (top), -1 maps to y=height (bottom), 0 at center.
/// Creates a filled envelope showing min/max values at each point.
pub fn generate_waveform_path(
    samples: &[i16],
    num_points: usize,
    width: f64,
    height: f64,
) -> String {
    if samples.is_empty() {
        let center_y = height / 2.0;
        return format!(
            "M 0 {} L {} {} L {} {} L 0 {} Z",
            center_y, width, center_y, width, center_y, center_y
        );
    }

    let num_points = num_points.max(2); // Need at least 2 points

    // We'll draw the upper envelope forward, then lower envelope backward to create a filled shape
    let mut upper_points: Vec<(f64, f64)> = Vec::with_capacity(num_points);
    let mut lower_points: Vec<(f64, f64)> = Vec::with_capacity(num_points);

    for i in 0..num_points {
        // Map i to sample range
        let t = i as f64 / (num_points - 1) as f64; // 0.0 to 1.0
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
        // x: 0 to width (full width)
        // y: 0 (top, +1.0) to height (bottom, -1.0), center at height/2
        let x = t * width;
        let center_y = height / 2.0;
        let y_range = height * 0.98 / 2.0; // Use 98% of height for waveform (slight padding)
        let y_max = center_y - (max_val as f64 / i16::MAX as f64) * y_range;
        let y_min = center_y - (min_val as f64 / i16::MAX as f64) * y_range;

        upper_points.push((x, y_max));
        lower_points.push((x, y_min));
    }

    // Build path: upper envelope forward, then lower envelope backward
    let mut path = String::with_capacity(num_points * 40);

    // Start at x=0 with first point
    path.push_str(&format!(
        "M 0 {:.1}",
        upper_points.first().map(|p| p.1).unwrap_or(500.0)
    ));

    // Draw upper envelope
    for &(x, y) in upper_points.iter() {
        path.push_str(&format!(" L {:.1} {:.1}", x, y));
    }

    // Ensure we reach x=width
    if let Some(&(_, y)) = upper_points.last() {
        path.push_str(&format!(" L {:.1} {:.1}", width, y));
    }

    // Draw lower envelope in reverse
    if let Some(&(_, y)) = lower_points.last() {
        path.push_str(&format!(" L {:.1} {:.1}", width, y));
    }
    for &(x, y) in lower_points.iter().rev() {
        path.push_str(&format!(" L {:.1} {:.1}", x, y));
    }

    // Close back to start
    path.push_str(&format!(
        " L 0 {:.1}",
        lower_points.first().map(|p| p.1).unwrap_or(500.0)
    ));
    path.push_str(" Z");

    path
}
