# fw16-ledmatrix-widgets

Two small daemons that turn the Framework Laptop 16 LED matrix modules into status displays,
meant to run as systemd user services.

| Daemon | Shows |
|---|---|
| `claude-matrix` | Claude plan usage (session and weekly), reset countdown, and whether Claude Code is working |
| `ai-matrix` | [zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) agent activity and [gufo](https://github.com/gufo-org/gufo) inference throughput |

Each daemon drives one module. Both follow a shared brightness setting that a single
keybind can cycle.

They talk to the stock firmware from
[FrameworkComputer/inputmodule-rs](https://github.com/FrameworkComputer/inputmodule-rs),
tested with version 0.2.0. No firmware changes are needed.

## Build and install

```sh
cargo install --path .
```

The binaries land in `~/.cargo/bin`. Your user needs access to the modules' serial devices,
see [Permissions on Linux](https://github.com/FrameworkComputer/inputmodule-rs#permissions-on-linux).

## Choosing a module

Both modules report the same USB serial number, so `/dev/serial/by-id` only holds one link
for the pair. Use `/dev/serial/by-path` instead, which is stable per USB port:

```sh
ls -l /dev/serial/by-path/
```

The services below are templates whose instance name is that by-path name, for example
`pci-0000:c1:00.3-usb-0:3.3:1.0`.

## claude-matrix

### Layout

| Rows | Content |
|---|---|
| 0-23 | Session bar (columns 0-3) and weekly bar (columns 5-8), filling from the bottom |
| 25-33 | The Claude spark: spinning while Claude Code works, dim and still when idle |

In each bar, bright pixels are the percentage used and dim pixels are how much of the window
has elapsed. Bright above dim means usage is outpacing the window.

When a limit is used up, the whole display shows the time until it resets instead, for
example `45M`, `14H05` or `3D14H`, centred vertically.

### Data sources

- **Usage** comes from Claude Code's
  [status line data](https://code.claude.com/docs/en/statusline#rate-limit-usage), which
  carries the plan's session and weekly limits for Pro and Max subscribers. The
  `claude-matrix statusline` command saves them to `$XDG_RUNTIME_DIR/claude-matrix-usage.json`
  whenever Claude Code updates its status line, so there is no polling while you use it.
- **Fallback usage**: when Claude Code hasn't reported for 10 minutes, for example after a
  reboot, the daemon polls `https://api.anthropic.com/api/oauth/usage`, the undocumented
  endpoint behind Claude Code's `/usage`, at most every 10 minutes. It uses the OAuth token
  in `~/.claude/.credentials.json`, or `$CLAUDE_CONFIG_DIR` if set, and never refreshes it.
  The newest data from either source wins.
- **Activity** comes from Claude Code hooks. Each session gets a marker file in
  `$XDG_RUNTIME_DIR/claude-matrix` while it works. Claude Code does not report Esc
  interrupts, so markers older than 10 minutes count as idle.

### Hooks

Add these to the `hooks` section of `~/.claude/settings.json`, next to any existing entries:

```json
"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "~/.cargo/bin/claude-matrix busy", "timeout": 5}]}],
"PreToolUse":       [{"hooks": [{"type": "command", "command": "~/.cargo/bin/claude-matrix busy", "timeout": 5}]}],
"PostToolUse":      [{"hooks": [{"type": "command", "command": "~/.cargo/bin/claude-matrix busy", "timeout": 5}]}],
"Stop":             [{"hooks": [{"type": "command", "command": "~/.cargo/bin/claude-matrix idle", "timeout": 5}]}],
"SessionEnd":       [{"hooks": [{"type": "command", "command": "~/.cargo/bin/claude-matrix idle", "timeout": 5}]}]
```

The hook commands read the session ID from the hook's JSON input and never fail, so a
broken setup can't get in Claude Code's way.

### Status line

Add this to `~/.claude/settings.json` so the daemon gets usage without polling:

```json
"statusLine": {"type": "command", "command": "~/.cargo/bin/claude-matrix statusline"}
```

It prints the limits as the status line, for example `session 24% · week 41%`. Like any
custom status line, it replaces most of Claude Code's footer keyboard hints. The limits
appear after the session's first response.

Only Claude Code's terminal interface runs status line commands. Editors that drive Claude
through the Agent Client Protocol, such as Zed, still run the hooks but never the status
line, so without a terminal session the daemon relies on the fallback polling.

### Service

```sh
cp systemd/claude-matrix@.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now 'claude-matrix@<by-path name>'
```

## ai-matrix

### Layout

| Rows | Columns | Content |
|---|---|---|
| 0-7 | 0-3 and 5-8 | One tile per zeroclaw agent, the first two by name |
| 0-7 | 4 | gufo sessions, stacking from the bottom |
| 9-33 | all | gufo throughput history, newest row at the bottom, one row per 3 seconds |

Agent tiles:

| Look | Meaning |
|---|---|
| Dark | zeroclaw unreachable |
| Dim outline | Idle |
| Bouncing bar | Working on a turn |
| Full flash | Tool call succeeded |
| Outline flash | Tool call failed |

In the session column, bright pixels are running requests and mid-bright pixels are
requests waiting for a session. In the history, bright pixels are generated tokens per second
and dim pixels are prompt tokens per second.

### Data sources

- **zeroclaw**: the gateway's event stream at `/api/events`, seeded from
  `/api/events/history` on each connection. Turns without an end event stop counting after
  an hour. The daemon reconnects on its own, and only logs when the error changes.
- **gufo**: the Prometheus text at `/metrics`, polled every row. Throughput is computed from
  `llamacpp:tokens_predicted_total` and `llamacpp:prompt_tokens_total`. Sessions come from
  `llamacpp:requests_processing` and `llamacpp:requests_deferred`.

Upstream gufo only updates its token counters when a request finishes, which shows as
occasional full-width rows, and has no session gauges. A gufo build that counts tokens as
they are produced and exports the session gauges makes the display live. Without the
gauges, the session column stays dark.

### zeroclaw token

The gateway requires a bearer token from its pairing flow. Exchange a one-time pairing code
and store the token where the daemon reads it:

```sh
mkdir -p -m 700 ~/.config/ai-matrix
curl -s -X POST -H 'X-Pairing-Code: <code>' https://<zeroclaw>/pair \
  | jq -r .token > ~/.config/ai-matrix/zeroclaw-token
chmod 600 ~/.config/ai-matrix/zeroclaw-token
```

### Service

The unit passes the zeroclaw and gufo URLs on the command line. Edit `ExecStart` in
`systemd/ai-matrix@.service` for your hosts, then:

```sh
cp systemd/ai-matrix@.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now 'ai-matrix@<by-path name>'
```

To run it by hand:

```sh
ai-matrix /dev/serial/by-path/<by-path name> https://<zeroclaw> https://<gufo>/metrics
```

## Brightness

Either binary cycles the shared brightness level, from low to medium to high to off and back
to low:

```sh
claude-matrix cycle-brightness
```

Running daemons pick up the change within a frame, and the level survives restarts. It is
stored in `$XDG_STATE_HOME/led-matrix/brightness`, by default
`~/.local/state/led-matrix/brightness`.

| Level | Dim pixels | Bright pixels |
|---|---|---|
| Low | 4 | 16 |
| Medium, the default | 8 | 48 |
| High | 12 | 128 |
| Off | 0 | 0 |

Values are LED output out of 255. The daemons scale every frame themselves and keep the
firmware at full brightness, so a level change is a single frame update without flashing.
Anything that would come out below 4 stays dark, because fainter output lights LEDs
unevenly.

A niri keybind, with the full path since niri may not have `~/.cargo/bin` on its `PATH`:

```kdl
Mod+Shift+B { spawn "/home/<user>/.cargo/bin/claude-matrix" "cycle-brightness"; }
```

## Tuning

The constants marked "Calibration knob" at the top of each source file are meant to be
adjusted by eye:

| File | Knobs |
|---|---|
| `src/matrix.rs` | Brightness levels, and the faintest output that lights LEDs evenly |
| `src/bin/claude-matrix.rs` | Spark spin speed |
| `src/bin/ai-matrix.rs` | Seconds per history row, tokens per second for a full row, queued-session brightness |

The countdown font is in `src/font.rs`. After a change, reinstall and restart the services.

## Troubleshooting

- **Logs**: `journalctl --user-unit 'claude-matrix@*' --user-unit 'ai-matrix@*'`. This
  also works on systems without a per-user journal, where `journalctl --user` finds nothing.
- **The service restarts after suspend or unplugging.** That's expected: a failed serial
  write ends the process and systemd starts it again after 5 seconds.
- **The display takes a few seconds to appear after resume.** The firmware fades in from
  sleep for up to about 5 seconds and doesn't read commands meanwhile. The daemons wait up
  to 10 seconds per write.
- **The firmware sleeps after 60 seconds without commands.** The daemons resend the frame
  every 30 seconds to keep their module awake.
- **Framework's `inputmodule-control` tool can't reach a module a daemon is using.** Each
  daemon opens its serial port exclusively. Stop the service first.

## Tests

```sh
cargo test
```
