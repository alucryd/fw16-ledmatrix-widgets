//! Show local AI activity on a Framework LED matrix: zeroclaw agents and gufo throughput.
//!
//!   ai-matrix <serial-dev> <zeroclaw-url> <gufo-metrics-url>
//!   ai-matrix cycle-brightness   next brightness level for all matrices, then off
//!
//! The zeroclaw bearer token is read from ~/.config/ai-matrix/zeroclaw-token, as returned
//! by zeroclaw's `POST /pair` with an `X-Pairing-Code` header.
//!
//! Layout (9x34):
//! - Rows 0-7: one tile per zeroclaw agent, first two by name, cols 0-3 and 5-8.
//!   Dark = zeroclaw unreachable, dim outline = idle, bouncing bar = working,
//!   full flash = tool call, outline flash = failed tool call.
//! - Col 4, rows 0-7, between the tiles: gufo sessions from the bottom up.
//!   Bright = running, mid = waiting for a session. Needs gufo's live metrics.
//! - Rows 9-33: gufo throughput history, newest row at the bottom, one row per ROW_SECS.
//!   Bright = generated tokens/s, dim = prompt tokens/s.

use chrono::{DateTime, Utc};
use fw16_ledmatrix_widgets::matrix::{self, DIM, Frame, HEIGHT, WIDTH};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

const TILE_ROWS: usize = 8;
const HIST_TOP: usize = 9;
const HIST_ROWS: usize = HEIGHT - HIST_TOP;
/// Calibration knob: queued sessions, must stay distinct from DIM tile outlines.
const QUEUED: u8 = 80;
/// Calibration knob: seconds per history row, 25 rows = 75 seconds.
const ROW_SECS: u64 = 3;
/// Calibration knobs: tokens/s that fill the full width.
const GEN_MAX: f64 = 60.0;
const PROMPT_MAX: f64 = 1500.0;
const GEN_METRIC: &str = "llamacpp:tokens_predicted_total";
const PROMPT_METRIC: &str = "llamacpp:prompt_tokens_total";
const RUNNING_METRIC: &str = "llamacpp:requests_processing";
const QUEUED_METRIC: &str = "llamacpp:requests_deferred";
/// Turns without an agent_end (zeroclaw restart, lost event) stop counting after this.
const STALE_TURN: chrono::TimeDelta = chrono::TimeDelta::hours(1);
const FLASH: Duration = Duration::from_millis(400);

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    generated: f64,
    prompt: f64,
    running: usize,
    queued: usize,
}

#[derive(Default)]
struct Agents {
    connected: bool,
    // ponytail: only the first two agents by name get a tile, a third needs a new layout.
    by_name: BTreeMap<String, Agent>,
}

#[derive(Default)]
struct Agent {
    /// turn_id -> start time
    turns: HashMap<String, DateTime<Utc>>,
    /// When the last tool call finished, and whether it succeeded.
    flash: Option<(Instant, bool)>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args == ["cycle-brightness"] {
        println!("brightness {}", matrix::cycle_brightness()?);
        return Ok(());
    }
    let [dev, zeroclaw, metrics] = &args[..] else {
        eprintln!("usage: ai-matrix <serial-dev> <zeroclaw-url> <gufo-metrics-url>");
        eprintln!("       ai-matrix cycle-brightness");
        std::process::exit(2)
    };
    let token_path = PathBuf::from(env::var_os("HOME").unwrap_or_default())
        .join(".config/ai-matrix/zeroclaw-token");
    let token = fs::read_to_string(&token_path)
        .map_err(|e| format!("{}: {e}", token_path.display()))?
        .trim()
        .to_string();

    let agents = Arc::new(Mutex::new(Agents::default()));
    let url = zeroclaw.trim_end_matches('/').to_string();
    let shared = agents.clone();
    thread::spawn(move || {
        let mut logged = String::new();
        loop {
            let err = watch(&url, &token, &shared).unwrap_err().to_string();
            let mut a = shared.lock().unwrap();
            // Off the home network this fails every few seconds, only log changes.
            if a.connected || err != logged {
                eprintln!("zeroclaw: {err}");
                logged = err;
            }
            a.connected = false;
            drop(a);
            thread::sleep(Duration::from_secs(5));
        }
    });

    let mut history = VecDeque::new();
    let mut last: Option<Sample> = None;
    let mut logged = String::new();
    let mut next_row = Instant::now();
    matrix::run(dev, |t| {
        if Instant::now() >= next_row {
            next_row += Duration::from_secs(ROW_SECS);
            let sample = match fetch_sample(metrics) {
                Ok(sample) => {
                    logged.clear();
                    Some(sample)
                }
                Err(e) if e.to_string() == logged => None,
                Err(e) => {
                    eprintln!("gufo: {e}");
                    logged = e.to_string();
                    None
                }
            };
            // Counters drop when gufo restarts, clamp instead of drawing a negative rate.
            let row = match (last, sample) {
                (Some(a), Some(b)) => {
                    let secs = (b.at - a.at).as_secs_f64();
                    let rate = |from: f64, to: f64| (to - from).max(0.0) / secs;
                    (rate(a.generated, b.generated), rate(a.prompt, b.prompt))
                }
                _ => (0.0, 0.0),
            };
            if last.is_some() || sample.is_none() {
                history.push_back(row);
                if history.len() > HIST_ROWS {
                    history.pop_front();
                }
            }
            last = sample;
        }
        render(
            &agents.lock().unwrap(),
            last.map(|s| (s.running, s.queued)),
            &history,
            t,
            Instant::now(),
            Utc::now(),
        )
    })
}

/// Seed state from the event history, then follow the live SSE stream until it drops.
fn watch(url: &str, token: &str, agents: &Mutex<Agents>) -> Result<(), Box<dyn Error>> {
    // Keepalive comments arrive well within this, so a silent network drop surfaces here.
    let http = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .build();
    let get = |path: &str| {
        http.get(&format!("{url}{path}"))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(Box::new)
    };
    let history: Value = serde_json::from_reader(get("/api/events/history")?.into_reader())?;
    let stream = get("/api/events")?;
    {
        let mut a = agents.lock().unwrap();
        *a = Agents::default();
        for ev in history["events"].as_array().into_iter().flatten() {
            apply(&mut a, ev, None);
        }
        a.connected = true;
    }
    for line in BufReader::new(stream.into_reader()).lines() {
        if let Some(Ok(ev)) = line?
            .strip_prefix("data:")
            .map(serde_json::from_str::<Value>)
        {
            apply(&mut agents.lock().unwrap(), &ev, Some(Instant::now()));
        }
    }
    Err("event stream closed".into())
}

/// `live` is None while replaying history, so old tool calls don't flash.
fn apply(agents: &mut Agents, ev: &Value, live: Option<Instant>) {
    let Some(name) = ev["agent_alias"].as_str() else {
        return;
    };
    let agent = agents.by_name.entry(name.to_string()).or_default();
    let turn = ev["turn_id"].as_str().unwrap_or_default().to_string();
    match ev["type"].as_str() {
        Some("agent_start") => {
            let start = ev["timestamp"]
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map_or_else(Utc::now, |t| t.with_timezone(&Utc));
            agent.turns.insert(turn, start);
        }
        Some("agent_end") => {
            agent.turns.remove(&turn);
        }
        Some("tool_call") => {
            if let Some(now) = live {
                agent.flash = Some((now, ev["success"].as_bool() != Some(false)));
            }
        }
        _ => {}
    }
}

fn fetch_sample(url: &str) -> Result<Sample, Box<dyn Error>> {
    let text = ureq::get(url)
        .timeout(Duration::from_secs(3))
        .call()?
        .into_string()?;
    let metric = |name: &str| -> Option<f64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.trim().parse().ok())
    };
    let counter = |name: &str| metric(name).ok_or(format!("{name} missing"));
    // Upstream gufo has no session gauges, show none rather than failing.
    let gauge = |name: &str| metric(name).unwrap_or(0.0).max(0.0) as usize;
    Ok(Sample {
        at: Instant::now(),
        generated: counter(GEN_METRIC)?,
        prompt: counter(PROMPT_METRIC)?,
        running: gauge(RUNNING_METRIC),
        queued: gauge(QUEUED_METRIC),
    })
}

fn render(
    agents: &Agents,
    sessions: Option<(usize, usize)>,
    history: &VecDeque<(f64, f64)>,
    t: f64,
    now: Instant,
    utc: DateTime<Utc>,
) -> Frame {
    let mut f = [[0; HEIGHT]; WIDTH];
    if agents.connected {
        for (i, agent) in agents.by_name.values().take(2).enumerate() {
            let busy = agent.turns.values().any(|start| utc - *start < STALE_TURN);
            let bar = (TILE_ROWS - 1) as f64 * (0.5 - 0.5 * (t * 3.0).cos());
            let flash = agent.flash.and_then(|(at, ok)| {
                let age = now.saturating_duration_since(at);
                (age < FLASH).then(|| (1.0 - age.as_secs_f64() / FLASH.as_secs_f64(), ok))
            });
            for (x, col) in f[i * 5..i * 5 + 4].iter_mut().enumerate() {
                for (y, px) in col[..TILE_ROWS].iter_mut().enumerate() {
                    let edge = x == 0 || x == 3 || y == 0 || y == TILE_ROWS - 1;
                    let mut v = if edge { DIM } else { 0 };
                    if busy {
                        let near = 1.0 - (y as f64 - bar).abs() / 1.5;
                        v = v.max((near.max(0.0) * 255.0) as u8);
                    }
                    if let Some((k, ok)) = flash
                        && (ok || edge)
                    {
                        v = v.max((k * 255.0) as u8);
                    }
                    *px = v;
                }
            }
        }
    }
    if let Some((running, queued)) = sessions {
        let lit = (0..running).map(|_| 255).chain((0..queued).map(|_| QUEUED));
        for (y, v) in (0..TILE_ROWS).rev().zip(lit) {
            f[WIDTH / 2][y] = v;
        }
    }
    let width = |rate: f64, max: f64| ((rate / max).min(1.0) * WIDTH as f64).ceil() as usize;
    for (i, &(generated, prompt)) in history.iter().rev().enumerate() {
        let (g, p) = (width(generated, GEN_MAX), width(prompt, PROMPT_MAX));
        for (x, col) in f.iter_mut().enumerate() {
            col[HEIGHT - 1 - i] = if x < g {
                255
            } else if x < p {
                DIM
            } else {
                0
            };
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str, agent: &str, turn: &str, extra: Value) -> Value {
        let mut v = json!({"type": kind, "agent_alias": agent, "turn_id": turn,
            "timestamp": Utc::now().to_rfc3339()});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    }

    #[test]
    fn turns_and_tool_calls_drive_tiles() {
        let mut a = Agents {
            connected: true,
            ..Default::default()
        };
        let (now, utc) = (Instant::now(), Utc::now());
        // Replayed history: glados still mid-turn, cortana finished, no flashes.
        apply(&mut a, &ev("agent_start", "glados", "g1", json!({})), None);
        apply(&mut a, &ev("agent_start", "cortana", "c1", json!({})), None);
        apply(&mut a, &ev("agent_end", "cortana", "c1", json!({})), None);
        apply(
            &mut a,
            &ev("tool_call", "cortana", "c1", json!({"success": true})),
            None,
        );
        assert!(a.by_name["cortana"].flash.is_none());
        let f = render(&a, None, &VecDeque::new(), 0.0, now, utc);
        // Cortana (left, idle): outline only. Glados (right, busy): bar at the top at t=0.
        assert_eq!((f[0][0], f[1][1], f[1][TILE_ROWS - 1]), (DIM, 0, DIM));
        assert_eq!(f[6][0], 255);
        assert_eq!(f[6][TILE_ROWS - 2], 0);

        // A live failed tool call flashes only the outline.
        let failed = json!({"success": false});
        apply(&mut a, &ev("tool_call", "cortana", "c2", failed), Some(now));
        let f = render(&a, None, &VecDeque::new(), 0.0, now, utc);
        assert_eq!((f[0][0], f[1][1]), (255, 0));
        apply(
            &mut a,
            &ev("tool_call", "cortana", "c2", json!({})),
            Some(now),
        );
        assert_eq!(render(&a, None, &VecDeque::new(), 0.0, now, utc)[1][1], 255);
        assert_eq!(
            render(&a, None, &VecDeque::new(), 0.0, now + FLASH, utc)[1][1],
            0
        );

        // Turns that never ended go stale, and a lost connection blanks the tiles.
        assert_eq!(
            render(&a, None, &VecDeque::new(), 0.0, now, utc + STALE_TURN * 2)[6][0],
            DIM
        );
        a.connected = false;
        let f = render(&a, None, &VecDeque::new(), 0.0, now, utc);
        assert!(f.iter().all(|col| col[..TILE_ROWS].iter().all(|&v| v == 0)));
    }

    #[test]
    fn sessions_stack_up_between_the_tiles() {
        let (a, now, utc) = (Agents::default(), Instant::now(), Utc::now());
        let column = |sessions| render(&a, sessions, &VecDeque::new(), 0.0, now, utc)[4];
        let col = column(Some((2, 1)));
        assert_eq!(col[..TILE_ROWS], [0, 0, 0, 0, 0, QUEUED, 255, 255]);
        assert!(column(Some((10, 3)))[..TILE_ROWS].iter().all(|&v| v == 255));
        assert!(
            column(None)[..TILE_ROWS].iter().all(|&v| v == 0),
            "gufo unreachable"
        );
    }

    #[test]
    fn history_rows_scroll_up_from_the_bottom() {
        let a = Agents::default();
        let history = VecDeque::from([(30.0, 1500.0), (0.0, 0.0), (0.1, 0.0)]);
        let f = render(&a, None, &history, 0.0, Instant::now(), Utc::now());
        let row = |y: usize| (0..WIDTH).map(|x| f[x][y]).collect::<Vec<_>>();
        // Newest: a trickle still lights one pixel.
        assert_eq!(row(HEIGHT - 1), [255, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(row(HEIGHT - 2), [0; 9]);
        // Oldest: half-speed generation over full-speed prompt processing.
        assert_eq!(
            row(HEIGHT - 3),
            [255, 255, 255, 255, 255, DIM, DIM, DIM, DIM]
        );
    }
}
