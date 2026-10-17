//! The tray icon, drawn in code.
//!
//! A speaker with one to three sound waves for the boost level, or a grey
//! speaker with a slash when processing is off. Drawing it rather than
//! shipping `.ico` files keeps the build free of a resource compiler, gives a
//! crisp icon at whatever size the shell asks for at the current DPI, and lets
//! the icon show the level at a glance.
//!
//! The output is plain `0xAARRGGBB` pixels, top row first, straight (not
//! premultiplied) alpha -- exactly what a 32-bit top-down DIB section expects
//! for `CreateIconIndirect`.

/// Speaker body and cone.
const SPEAKER_ON: u32 = 0xFF2B7CD3;
const SPEAKER_OFF: u32 = 0xFF8C8C8C;
/// Waves, from the first (always shown) to the third (only at high boost).
const WAVES: [u32; 3] = [0xFF2BA84A, 0xFFE0A100, 0xFFE0482B];
/// The "off" slash.
const SLASH: u32 = 0xFFD03B3B;

/// Samples per pixel side; 4 x 4 gives smooth edges at 16 px.
const SUPERSAMPLE: u32 = 4;

/// How many waves a slider position earns.
pub fn wave_count(percent: u32) -> usize {
    match percent {
        0..=150 => 1,
        151..=300 => 2,
        _ => 3,
    }
}

/// Render the icon at `size` x `size` pixels.
pub fn render(size: u32, enabled: bool, percent: u32) -> Vec<u32> {
    let size = size.max(1);
    let waves = if enabled { wave_count(percent) } else { 0 };
    let mut pixels = Vec::with_capacity((size * size) as usize);

    for row in 0..size {
        for column in 0..size {
            // Accumulate coverage per colour, then resolve to one colour with
            // the summed alpha. Shapes never overlap except the slash, which
            // is drawn on top.
            let mut hits = [0u32; 5];
            for sub_row in 0..SUPERSAMPLE {
                for sub_column in 0..SUPERSAMPLE {
                    let x = (column * SUPERSAMPLE + sub_column) as f32 + 0.5;
                    let y = (row * SUPERSAMPLE + sub_row) as f32 + 0.5;
                    let scale = (size * SUPERSAMPLE) as f32;
                    if let Some(shape) = shape_at(x / scale, y / scale, enabled, waves) {
                        hits[shape] += 1;
                    }
                }
            }
            pixels.push(resolve(&hits, enabled));
        }
    }
    pixels
}

/// Which shape covers a point in unit coordinates: 0 speaker, 1..=3 waves,
/// 4 slash.
fn shape_at(x: f32, y: f32, enabled: bool, waves: usize) -> Option<usize> {
    if !enabled && on_slash(x, y) {
        return Some(4);
    }
    if in_speaker(x, y) {
        return Some(0);
    }
    (0..waves)
        .find(|&wave| on_wave(x, y, wave))
        .map(|wave| wave + 1)
}

fn in_speaker(x: f32, y: f32) -> bool {
    let body = (0.06..=0.28).contains(&x) && (0.35..=0.65).contains(&y);
    // The cone flares from the body's height at x = 0.28 to most of the icon
    // at x = 0.50.
    let flare = 0.15 + (x - 0.28) / 0.22 * 0.22;
    let cone = (0.28..=0.50).contains(&x) && (y - 0.5).abs() <= flare;
    body || cone
}

fn on_wave(x: f32, y: f32, wave: usize) -> bool {
    const RADII: [f32; 3] = [0.20, 0.32, 0.44];
    const HALF_THICKNESS: f32 = 0.045;
    let (dx, dy) = (x - 0.50, y - 0.50);
    if dx <= 0.0 {
        return false;
    }
    let radius = (dx * dx + dy * dy).sqrt();
    let angle = dy.atan2(dx).abs();
    (radius - RADII[wave]).abs() <= HALF_THICKNESS && angle <= 50f32.to_radians()
}

fn on_slash(x: f32, y: f32) -> bool {
    // Distance to the line from bottom-left to top-right, x + y = 1.
    let distance = (x + y - 1.0).abs() / std::f32::consts::SQRT_2;
    distance <= 0.07 && (0.06..=0.94).contains(&x)
}

fn resolve(hits: &[u32; 5], enabled: bool) -> u32 {
    let total: u32 = hits.iter().sum();
    if total == 0 {
        return 0;
    }
    let (shape, _) = hits
        .iter()
        .enumerate()
        .max_by_key(|(_, count)| **count)
        .expect("five entries");
    let colour = match shape {
        0 if enabled => SPEAKER_ON,
        0 => SPEAKER_OFF,
        4 => SLASH,
        wave => WAVES[wave - 1],
    };
    let samples = SUPERSAMPLE * SUPERSAMPLE;
    let alpha = (255 * total.min(samples) + samples / 2) / samples;
    (alpha << 24) | (colour & 0x00FF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque_right_half(pixels: &[u32], size: u32) -> usize {
        pixels
            .iter()
            .enumerate()
            .filter(|(index, pixel)| (*index as u32 % size) > size / 2 && **pixel >> 24 > 128)
            .count()
    }

    #[test]
    fn every_size_the_shell_asks_for_has_the_right_pixel_count() {
        for size in [16, 20, 24, 32, 40, 48, 64] {
            assert_eq!(render(size, true, 300).len(), (size * size) as usize);
        }
    }

    #[test]
    fn corners_are_transparent() {
        let size = 32;
        let pixels = render(size, true, 500);
        for index in [0, size - 1, size * (size - 1), size * size - 1] {
            assert_eq!(pixels[index as usize] >> 24, 0, "corner {index}");
        }
    }

    #[test]
    fn more_boost_draws_more_waves() {
        let size = 32;
        let low = opaque_right_half(&render(size, true, 100), size);
        let mid = opaque_right_half(&render(size, true, 250), size);
        let high = opaque_right_half(&render(size, true, 500), size);
        assert!(low < mid && mid < high, "{low} {mid} {high}");
    }

    #[test]
    fn off_has_no_waves_and_a_slash() {
        let pixels = render(32, false, 500);
        let colours: Vec<u32> = pixels
            .iter()
            .filter(|pixel| **pixel >> 24 > 0)
            .map(|pixel| pixel | 0xFF00_0000)
            .collect();
        for wave in WAVES {
            assert!(!colours.contains(&wave), "wave {wave:#x} drawn while off");
        }
        assert!(colours.contains(&SLASH));
        assert!(colours.contains(&SPEAKER_OFF));
        assert!(!colours.contains(&SPEAKER_ON));
    }

    #[test]
    fn wave_thresholds() {
        assert_eq!(wave_count(100), 1);
        assert_eq!(wave_count(150), 1);
        assert_eq!(wave_count(151), 2);
        assert_eq!(wave_count(300), 2);
        assert_eq!(wave_count(301), 3);
        assert_eq!(wave_count(500), 3);
    }
}
