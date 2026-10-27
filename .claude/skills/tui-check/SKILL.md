---
name: tui-check
description: Run the mtr-ng TUI headlessly in tmux (simulation mode) and read the screen as text, at chosen terminal sizes and after key presses. Use after any change under src/ui/, to reproduce a layout or rendering bug, or to check behaviour on resize.
---

# Headless TUI check

The TUI can't be watched directly, so run it in a detached tmux session and capture the pane. The helper uses `--simulate` unless `MTR_REAL=1` (real probes need no root on macOS) and runs in a temp dir so `mtr-ng.log` doesn't land in the repo.

```sh
cargo build
.claude/skills/tui-check/capture.sh 120x30 3                 # size, seconds to wait
.claude/skills/tui-check/capture.sh 120x30 3 o               # then press 'o' (column selector)
.claude/skills/tui-check/capture.sh 120x30 3 resize=40x10    # resize mid-run, then capture
```

Arguments after the wait are sent in order: a `resize=WxH` token resizes the window, anything else goes to `tmux send-keys` (so `Escape`, `Left`, `Space`, `q` work). Env: `MTR_TARGET` (default `example.com`), `MTR_ARGS` (extra flags), `MTR_REAL=1` (real network).

Try at least one wide (200x50), one typical (120x30) and one cramped (40x10) size. Look for: a panic or shell prompt instead of the table, truncated headers or columns, misaligned numbers, stray `?` glyphs, and the pane printing `[exited]` when it shouldn't have.
