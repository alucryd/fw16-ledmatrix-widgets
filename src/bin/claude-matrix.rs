//! Show Claude plan usage and Claude Code activity on a Framework LED matrix.
//!
//!   claude-matrix run <serial-dev>   daemon, e.g. /dev/serial/by-path/pci-..._1.0
//!   claude-matrix busy | idle        Claude Code hooks, read the hook JSON on stdin
//!   claude-matrix statusline         Claude Code status line, saves the plan limits it reports
//!   claude-matrix cycle-brightness   next brightness level for all matrices, then off
//!
//! Layout (9x34): rows 0-23 hold the session bar (cols 0-3) and the weekly bar (cols 5-8).
//! Bright = % used, dim = time elapsed in the window. Rows 25-33 hold the Claude spark,
//! spinning while Claude works and dim when idle. When a limit is used up, the whole
//! display shows the time until it resets instead.

use chrono::{DateTime, Utc};
use fw16_ledmatrix_widgets::font;
use fw16_ledmatrix_widgets::matrix::{self, Frame, HEIGHT, WIDTH};
use serde_json::Value;
use std::error::Error;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use std::{env, fs};

const BAR_ROWS: usize = 24;
const SPARK_TOP: usize = 25;
/// Calibration knob: spin speed in rad/s, a quarter turn per second.
const SPIN: f64 = std::f64::consts::FRAC_PI_2;
const USED: u8 = 255;
/// Elapsed time and the idle spark; see matrix::LEVELS for how bright that ends up.
const ELAPSED: u8 = matrix::DIM;
/// Fallback polling of the usage endpoint, only while Claude Code's status line hasn't
/// reported for this long. The endpoint rate-limits frequent polling.
const POLL: Duration = Duration::from_secs(600);
// ponytail: Stop doesn't fire on Esc interrupts, so busy markers expire instead.
// A single tool call running longer than this will show idle early.
const BUSY_STALE: Duration = Duration::from_secs(600);
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// When the limits were observed, and the session and weekly limits.
type Report = (SystemTime, [Option<Limit>; 2]);

#[derive(Clone, Copy)]
struct Limit {
    used: f64,
    resets_at: DateTime<Utc>,
    window_secs: f64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["run", dev] => run(dev),
        ["cycle-brightness"] => {
            println!("brightness {}", matrix::cycle_brightness()?);
            Ok(())
        }
        ["busy"] => hook(true),
        ["idle"] => hook(false),
        ["statusline"] => statusline(),
        _ => {
            eprintln!(
                "usage: claude-matrix run <serial-dev> | busy | idle | statusline | cycle-brightness"
            );
            std::process::exit(2)
        }
    }
}

fn run(dev: &str) -> Result<(), Box<dyn Error>> {
    let mut polled: Option<Report> = None;
    let mut last_poll: Option<Instant> = None;
    let mut logged = String::new();
    matrix::run(dev, |t| {
        let reported = read_reported();
        let recent = |r: &Report| r.0.elapsed().is_ok_and(|age| age < POLL);
        if !reported.as_ref().is_some_and(recent) && last_poll.is_none_or(|p| p.elapsed() >= POLL) {
            last_poll = Some(Instant::now());
            match fetch() {
                Ok(l) => {
                    polled = Some((SystemTime::now(), l));
                    logged.clear();
                }
                // A rate-limited token keeps failing the same way, only log changes.
                Err(e) if e.to_string() == logged => {}
                Err(e) => {
                    eprintln!("usage fetch failed: {e}");
                    logged = e.to_string();
                }
            }
        }
        let limits = newest(reported, polled).map_or([None, None], |r| r.1);
        render(&limits, is_busy(), t, Utc::now())
    })
}

fn newest(a: Option<Report>, b: Option<Report>) -> Option<Report> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.0 >= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn render(limits: &[Option<Limit>; 2], busy: bool, t: f64, now: DateTime<Utc>) -> Frame {
    let mut f = [[0; HEIGHT]; WIDTH];
    let blocked_until = limits
        .iter()
        .flatten()
        .filter(|l| l.used >= 100.0 && l.resets_at > now)
        .map(|l| l.resets_at)
        .max();
    if let Some(reset) = blocked_until {
        // Claude can't be working while locked out, so the text may use the spark rows.
        let text = fmt_remaining((reset - now).num_seconds());
        // One blank row between glyphs, the whole block centred on the display.
        let pitch = font::SIZE + 1;
        let top = (HEIGHT - (text.len() * pitch - 1)) / 2;
        let left = (WIDTH - font::SIZE) / 2;
        for (i, c) in text.chars().enumerate() {
            let glyph = font::glyph(c);
            for y in 0..font::SIZE {
                for x in 0..font::SIZE {
                    if glyph[x + y * font::SIZE] == 1 {
                        f[left + x][top + i * pitch + y] = USED;
                    }
                }
            }
        }
        return f;
    }
    for (i, l) in limits.iter().enumerate() {
        // A window that already reset shows empty until the API says otherwise.
        let Some(l) = l.filter(|l| l.resets_at > now) else {
            continue;
        };
        let used = (l.used / 100.0 * BAR_ROWS as f64).ceil() as usize;
        let left = (l.resets_at - now).num_seconds() as f64 / l.window_secs;
        let elapsed = ((1.0 - left).clamp(0.0, 1.0) * BAR_ROWS as f64).round() as usize;
        for k in 0..BAR_ROWS {
            let v = if k < used {
                USED
            } else if k < elapsed {
                ELAPSED
            } else {
                0
            };
            for col in &mut f[i * 5..i * 5 + 4] {
                col[BAR_ROWS - 1 - k] = v;
            }
        }
    }
    let (angle, peak) = if busy {
        (t * SPIN, 255.0)
    } else {
        (0.0, ELAPSED as f64)
    };
    for (x, col) in f.iter_mut().enumerate() {
        for y in 0..9 {
            col[SPARK_TOP + y] = (spark(x as f64 - 4.0, y as f64 - 4.0, angle) * peak) as u8;
        }
    }
    f
}

/// Brightness 0-1 of an 8-ray star rotated by `angle`, at offset (dx, dy) from its center.
/// Greyscale anti-aliasing is what keeps the rotation smooth at 9x9.
fn spark(dx: f64, dy: f64, angle: f64) -> f64 {
    let r = dx.hypot(dy);
    if r > 4.6 {
        return 0.0;
    }
    // 8 rays = 4 lines through the center, 45 degrees apart.
    let step = std::f64::consts::FRAC_PI_4;
    let a = (dy.atan2(dx) - angle).rem_euclid(step);
    (1.0 - r * a.min(step - a).sin() / 0.8).max(0.0)
}

/// At most 5 characters: 5 glyphs and their gaps take 29 of the 34 rows.
fn fmt_remaining(secs: i64) -> String {
    let m = (secs.max(0) + 59) / 60;
    let (d, h, mm) = (m / 1440, m / 60 % 24, m % 60);
    let options = if d > 0 {
        [format!("{d}D{h}H"), format!("{d}D")]
    } else if h > 0 {
        [format!("{h}H{mm:02}"), format!("{h}H")]
    } else {
        [format!("{mm}M"), String::new()]
    };
    // Only a window longer than 9 days would need the short form.
    options.into_iter().find(|s| s.len() <= 5).unwrap()
}

fn fetch() -> Result<[Option<Limit>; 2], Box<dyn Error>> {
    // Re-read every time: Claude Code refreshes the token and rewrites this file.
    // ponytail: undocumented endpoint, the same one Claude Code's /usage uses.
    let creds: Value =
        serde_json::from_str(&fs::read_to_string(config_dir().join(".credentials.json"))?)?;
    let token = creds["claudeAiOauth"]["accessToken"]
        .as_str()
        .ok_or("no OAuth token in credentials, log in with Claude Code")?;
    let body = ureq::get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .timeout(Duration::from_secs(10))
        .call()?
        .into_string()?;
    let v: Value = serde_json::from_str(&body)?;
    Ok([
        limit(&v["five_hour"], 5.0),
        limit(&v["seven_day"], 7.0 * 24.0),
    ])
}

fn limit(v: &Value, window_hours: f64) -> Option<Limit> {
    Some(Limit {
        used: v["utilization"].as_f64()?,
        resets_at: DateTime::parse_from_rfc3339(v["resets_at"].as_str()?)
            .ok()?
            .with_timezone(&Utc),
        window_secs: window_hours * 3600.0,
    })
}

/// Claude Code status line command. Saves the plan limits Claude Code reports, which the
/// daemon prefers over polling, and prints them. Never fails, like the hooks.
fn statusline() -> Result<(), Box<dyn Error>> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let limits = &v["rate_limits"];
    // Absent until the session's first API response, keep the last report meanwhile.
    if limits.is_object() {
        let path = usage_file();
        // Per process, so parallel sessions never rename each other's half-written file.
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let _ = fs::write(&tmp, limits.to_string()).and_then(|_| fs::rename(&tmp, &path));
    }
    println!("{}", status_text(limits));
    Ok(())
}

fn status_text(limits: &Value) -> String {
    [("five_hour", "session"), ("seven_day", "week")]
        .into_iter()
        .filter_map(|(window, label)| {
            let used = limits[window]["used_percentage"].as_f64()?;
            Some(format!("{label} {used:.0}%"))
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

fn read_reported() -> Option<Report> {
    let path = usage_file();
    let at = fs::metadata(&path).ok()?.modified().ok()?;
    let v: Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
    Some((at, reported_limits(&v)))
}

/// The status line's `rate_limits`: percentages and Unix-seconds reset times.
fn reported_limits(v: &Value) -> [Option<Limit>; 2] {
    let limit = |window: &str, hours: f64| {
        Some(Limit {
            used: v[window]["used_percentage"].as_f64()?,
            resets_at: DateTime::from_timestamp(v[window]["resets_at"].as_f64()? as i64, 0)?,
            window_secs: hours * 3600.0,
        })
    };
    [limit("five_hour", 5.0), limit("seven_day", 7.0 * 24.0)]
}

fn config_dir() -> PathBuf {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".claude"))
}

fn runtime_dir() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

/// One marker file per Claude Code session, so parallel sessions don't clear each other.
fn busy_dir() -> PathBuf {
    runtime_dir().join("claude-matrix")
}

/// Outside busy_dir, where every entry counts as a working session.
fn usage_file() -> PathBuf {
    runtime_dir().join("claude-matrix-usage.json")
}

fn is_busy() -> bool {
    let Ok(entries) = fs::read_dir(busy_dir()) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < BUSY_STALE))
    })
}

/// Never fails: a broken hook must not get in Claude Code's way.
fn hook(busy: bool) -> Result<(), Box<dyn Error>> {
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);
    let v: Value = serde_json::from_str(&input).unwrap_or_default();
    let id = v["session_id"].as_str().unwrap_or("unknown");
    // The id becomes a file name, so refuse anything that could escape the directory.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Ok(());
    }
    let path = busy_dir().join(id);
    if busy {
        let _ = fs::create_dir_all(busy_dir()).and_then(|_| fs::write(path, ""));
    } else {
        let _ = fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(now: DateTime<Utc>, used: f64, left_secs: i64, window_hours: f64) -> Option<Limit> {
        Some(Limit {
            used,
            resets_at: now + chrono::Duration::seconds(left_secs),
            window_secs: window_hours * 3600.0,
        })
    }

    #[test]
    fn countdown_block_is_centred() {
        let now = Utc::now();
        // "2H05": 4 glyphs and 3 gaps make 23 rows, leaving 5 above and 6 below.
        let f = render(
            &[at(now, 100.0, 2 * 3600 + 5 * 60, 5.0), None],
            false,
            0.0,
            now,
        );
        let rows: Vec<usize> = (0..HEIGHT)
            .filter(|&y| f.iter().any(|col| col[y] != 0))
            .collect();
        assert_eq!((rows[0], rows[rows.len() - 1]), (5, 27));

        // "3D14H": 5 glyphs make 29 rows, leaving 2 above and 3 below.
        let f = render(
            &[None, at(now, 100.0, 3 * 86400 + 14 * 3600, 168.0)],
            false,
            0.0,
            now,
        );
        let rows: Vec<usize> = (0..HEIGHT)
            .filter(|&y| f.iter().any(|col| col[y] != 0))
            .collect();
        assert_eq!((rows[0], rows[rows.len() - 1]), (2, 30));
    }

    #[test]
    fn status_line_limits_are_parsed_and_printed() {
        let v: Value = serde_json::from_str(
            r#"{"five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600}}"#,
        )
        .unwrap();
        let [session, week] = reported_limits(&v);
        let (session, week) = (session.unwrap(), week.unwrap());
        assert_eq!((session.used, week.used), (23.5, 41.2));
        assert_eq!(session.resets_at.timestamp(), 1738425600);
        assert_eq!(week.window_secs, 7.0 * 24.0 * 3600.0);
        assert_eq!(status_text(&v), "session 24% · week 41%");

        // Claude Code drops a window once it resets.
        let v: Value = serde_json::from_str(
            r#"{"seven_day": {"used_percentage": 5, "resets_at": 1738857600}}"#,
        )
        .unwrap();
        assert!(reported_limits(&v)[0].is_none());
        assert_eq!(status_text(&v), "week 5%");
        assert_eq!(status_text(&Value::Null), "");
    }

    #[test]
    fn the_newest_report_wins() {
        let now = Utc::now();
        let early = (SystemTime::UNIX_EPOCH, [at(now, 10.0, 60, 5.0), None]);
        let late = (SystemTime::now(), [at(now, 20.0, 60, 5.0), None]);
        let used = |r: Option<Report>| r.unwrap().1[0].unwrap().used;
        assert_eq!(used(newest(Some(early), Some(late))), 20.0);
        assert_eq!(used(newest(Some(late), Some(early))), 20.0);
        assert_eq!(used(newest(None, Some(early))), 10.0);
        assert!(newest(None, None).is_none());
    }

    #[test]
    fn remaining_fits_five_glyphs() {
        assert_eq!(fmt_remaining(45 * 60), "45M");
        assert_eq!(fmt_remaining(2 * 3600 + 5 * 60), "2H05");
        assert_eq!(fmt_remaining(14 * 3600 + 5 * 60), "14H05");
        assert_eq!(fmt_remaining(3 * 86400 + 4 * 3600), "3D4H");
        assert_eq!(fmt_remaining(3 * 86400 + 14 * 3600), "3D14H");
        assert_eq!(fmt_remaining(12 * 86400 + 14 * 3600), "12D");
    }

    #[test]
    fn bars_show_used_and_elapsed() {
        let now = Utc::now();
        // Session: 50% used, 4 of 5 hours elapsed. Weekly: nothing active.
        let f = render(&[at(now, 50.0, 3600, 5.0), None], false, 0.0, now);
        let lit = |v| (0..BAR_ROWS).filter(|&y| f[0][y] == v).count();
        assert_eq!(lit(USED), 12);
        assert_eq!(lit(ELAPSED), 19 - 12);
        assert_eq!(f[0][BAR_ROWS - 1], USED, "bars fill from the bottom");
        assert!((0..BAR_ROWS).all(|y| f[4][y] == 0 && f[8][y] == 0));
    }

    #[test]
    fn spark_spins_when_busy_and_dims_when_idle() {
        let now = Utc::now();
        let busy = |t| render(&[None, None], true, t, now);
        let center = SPARK_TOP + 4;
        assert_eq!(busy(0.0)[4][center], 255);
        assert_eq!(busy(0.0)[8][center], 255, "horizontal ray at angle 0");
        assert_ne!(busy(0.0), busy(0.2), "rotates over time");
        let eighth_turn = std::f64::consts::FRAC_PI_4 / SPIN;
        assert!(busy(eighth_turn)[8][center] >= 254, "8-fold symmetry");
        let idle = render(&[None, None], false, 5.0, now);
        assert_eq!(
            idle,
            render(&[None, None], false, 0.0, now),
            "idle is still"
        );
        assert_eq!(idle[4][center], ELAPSED);
        assert!(idle.iter().flatten().all(|&v| v <= ELAPSED));
    }

    #[test]
    fn exhausted_limit_shows_later_reset_as_text() {
        let now = Utc::now();
        let limits = [at(now, 100.0, 3600, 5.0), at(now, 100.0, 2 * 86400, 168.0)];
        let text = render(&limits, false, 0.0, now);
        let expected = render(&[None, at(now, 100.0, 2 * 86400, 168.0)], false, 0.0, now);
        assert_eq!(text, expected);
        assert!(
            text[0].iter().all(|&v| v == 0),
            "glyphs in cols 2-6, no spark"
        );
        // Expired limits fall back to empty bars.
        let expired = render(&[at(now, 100.0, -5, 5.0), None], false, 0.0, now);
        assert!((0..WIDTH).all(|x| expired[x][..BAR_ROWS].iter().all(|&v| v == 0)));
    }
}
