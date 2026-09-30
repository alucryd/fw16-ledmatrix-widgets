//! Frame loop, brightness levels and serial protocol shared by both daemons.

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

pub const WIDTH: usize = 9;
pub const HEIGHT: usize = 34;
pub type Frame = [[u8; HEIGHT]; WIDTH];

const FPS: u64 = 15;
/// The firmware sleeps after 60s without commands.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Pixel value both daemons use for dim elements (outlines, elapsed time, idle spark).
pub const DIM: u8 = 24;
/// Calibration knob: LED output (0-255) of DIM and full-bright pixels at each level of
/// `cycle-brightness`, off comes after the last one. Values in between scale linearly.
const LEVELS: [Level; 3] = [
    Level { dim: 4, bright: 16 },
    Level { dim: 8, bright: 48 },
    Level {
        dim: 12,
        bright: 128,
    },
];
/// Used until the first `cycle-brightness`, and for unknown values in the state file.
const DEFAULT_LEVEL: Level = LEVELS[1];
/// Calibration knob: faintest LED output that lights every LED evenly. Anything fainter,
/// like anti-aliasing fringes at low levels, stays dark rather than flickering unevenly.
const MIN_OUTPUT: u32 = 4;

#[derive(Clone, Copy)]
struct Level {
    dim: u8,
    bright: u8,
}

/// Shared by every daemon, so one keybind cycles all matrices together.
fn brightness_file() -> PathBuf {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("led-matrix/brightness")
}

/// The state file holds the level's bright output, 0 for off.
fn brightness() -> u8 {
    fs::read_to_string(brightness_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(DEFAULT_LEVEL.bright)
}

fn level(brightness: u8) -> Option<Level> {
    (brightness > 0).then(|| {
        LEVELS
            .into_iter()
            .find(|l| l.bright == brightness)
            .unwrap_or(DEFAULT_LEVEL)
    })
}

fn next_level(current: u8) -> u8 {
    LEVELS
        .into_iter()
        .map(|l| l.bright)
        .find(|&b| b > current)
        .unwrap_or(0)
}

/// Step to the next brightness level, from the highest to off, and from off to the lowest.
/// Running daemons pick it up within a frame.
pub fn cycle_brightness() -> io::Result<u8> {
    let next = next_level(brightness());
    let path = brightness_file();
    fs::create_dir_all(path.parent().unwrap_or(&path))?;
    // Rename so a daemon never reads a half-written file.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, next.to_string())?;
    fs::rename(tmp, path)?;
    Ok(next)
}

/// Call `render` with the seconds since start at 15 fps and send changed frames.
/// Write errors (unplug, suspend) end the loop; the systemd unit restarts the daemon.
pub fn run(dev: &str, mut render: impl FnMut(f64) -> Frame) -> Result<(), Box<dyn Error>> {
    let mut port = serialport::new(dev, 115_200)
        // Waking from sleep, the firmware fades in for up to ~5s without reading USB.
        .timeout(Duration::from_secs(10))
        .open()?;
    // Brightness is applied to each frame instead. Changing the firmware's level redraws
    // the previous frame at the new level before the next frame arrives, which flashes.
    port.write_all(&[0x32, 0xAC, 0x00, 255])?;
    let start = Instant::now();
    let mut last_frame = None;
    let mut last_sent = start;
    loop {
        let mut frame = render(start.elapsed().as_secs_f64());
        // A tiny file read per frame is cheaper than any file watching setup.
        scale(&mut frame, level(brightness()));
        if last_frame != Some(frame) || last_sent.elapsed() >= KEEPALIVE {
            send(&mut port, &frame)?;
            last_frame = Some(frame);
            last_sent = Instant::now();
        }
        thread::sleep(Duration::from_millis(1000 / FPS));
    }
}

/// Map pixel values to LED output: 0 to DIM onto 0 to the level's dim output, and DIM to
/// 255 onto dim to bright. `None` is off.
fn scale(frame: &mut Frame, level: Option<Level>) {
    for v in frame.iter_mut().flatten() {
        let Some(Level { dim, bright }) = level else {
            *v = 0;
            continue;
        };
        let (px, lo, d, b) = (
            u32::from(*v),
            u32::from(DIM),
            u32::from(dim),
            u32::from(bright),
        );
        let out = if px <= lo {
            px * d / lo
        } else {
            d + (px - lo) * (b - d) / (255 - lo)
        };
        *v = if out < MIN_OUTPUT { 0 } else { out as u8 };
    }
}

fn send(port: &mut impl Write, frame: &Frame) -> io::Result<()> {
    for (x, col) in frame.iter().enumerate() {
        // StageGreyCol, then DrawGreyColBuffer so the matrix never shows half a frame.
        port.write_all(&[&[0x32, 0xAC, 0x07, x as u8][..], col].concat())?;
    }
    port.write_all(&[0x32, 0xAC, 0x08])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_cycle_through_off() {
        let mut seen = vec![];
        let mut current = DEFAULT_LEVEL.bright;
        for _ in 0..4 {
            current = next_level(current);
            seen.push(current);
        }
        assert_eq!(seen, [128, 0, 16, 48]);
        // A stale value, like the old medium of 51, shows as medium and cycles on to high.
        assert_eq!(level(51).map(|l| l.bright), Some(48));
        assert_eq!(next_level(51), 128);
    }

    #[test]
    fn levels_map_dim_and_bright_pixels() {
        let out = |v: u8, brightness: u8| {
            let mut f = [[v; HEIGHT]; WIDTH];
            scale(&mut f, level(brightness));
            f[0][0]
        };
        for (brightness, dim, bright) in [(16, 4, 16), (48, 8, 48), (128, 12, 128)] {
            assert_eq!((out(DIM, brightness), out(255, brightness)), (dim, bright));
            // Values in between stay in between.
            let mid = out(128, brightness);
            assert!(dim < mid && mid < bright, "{mid} at {brightness}");
            // Faint fringes are dropped rather than lit unevenly.
            assert_eq!(out(3, brightness), 0);
        }
        assert_eq!((out(DIM, 0), out(255, 0)), (0, 0), "off");
    }
}
