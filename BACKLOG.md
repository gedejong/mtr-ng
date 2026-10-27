# mtr-ng backlog

Compiled 2026-10-27 from a full review (networking core, statistics/reporting, TUI, portability/CI, feature research). Line numbers refer to commit `59605fa`.

**Status:** milestones 1 and 2 are done. Ticked items are fixed; *italic notes* record partial progress.

Priority: **P0** shows wrong data or doesn't work, **P1** measurement correctness, **P2** visible defects, **P3** cleanup. Effort: S (hours), M (days), L (a week or more).

## Quality snapshot

The crate builds, the TUI looks good (sparklines, colour modes, a live column selector), and there is real release infrastructure. Underneath, much of what it measures is wrong. Unless the target is on a LAN, a real-network run today can show a fabricated path, stop at hop 10, report RTTs close to the probe interval, or never finish.

Structurally:
- There are two separate probing pipelines (report mode and interactive mode) that disagree on timeouts, protocol handling and loss accounting.
- Neither can be tested without root, because `ProbeEngine` is a concrete struct with no seam.
- The UI is one monolithic `render_ui` holding the session mutex for the whole frame. Knowledge about each column is spread across about seven parallel `match Column` tables that already disagree.
- A panic anywhere leaves the user's terminal in raw mode.

Tests and CI:
- The 34 unit tests cover `HopStats` and helpers in lockstep scenarios only. There are no parser, session, report, multipath or UI tests, and one test (`hop_stats.rs:545`) locks in a bug.
- CI on `master` is red (`cargo fmt --check`, and 9 clippy warnings under `-D warnings`).

Distribution is in worse shape than the code:
- A fresh dependency resolve no longer compiles.
- Every install channel (the one-liner, `install.sh`, Homebrew, the AUR) is broken.
- The release workflow targets a retired macOS runner.

## P0: wrong data or unusable

- [x] **B1 Silent fallback to fabricated traces.** IPv6 targets (`session.rs:92-95`, `:490-493`) and interactive runs without root (`:484-487`) switch to simulation. The only notice is a WARN in the log file, and nothing in the UI shows it. `mtr-ng -r -c 2 2001:4860:4860::8888` prints an invented IPv4 path and exits 0. Fix: error out unless `--simulate` is given; stamp SIMULATED in the TUI and the report. *S*
- [x] **B2 Real traces stop at hop 10 and never detect the target.** `num_hosts: 10` (`session.rs:82`) is never updated; `min(10, max_hops)` at `:186` and `:541`.
  - Paths longer than 10 hops are never completed, and `-M` has no effect.
  - Rows past a near target repeat the target, and Destination Unreachable doesn't end the path (`:315-324`).
  - `mark_as_target`/`is_target` are unused.
  - Fix: grow toward `max_hops`, and trim at the first hop whose source == target, or at a DU. *M*
- [x] **B3 Report-mode RTT and loss are wrong beyond ~50 ms.** *(m2, via R1. A 14-hop path to Japan went from ~1005 ms and 33% loss to 242-266 ms and 0%.)*
  - The collection window is hard-coded to 50 ms (`session.rs:236`; `collect_duration` is ignored). The loop then sleeps without reading the socket (`:161-168`).
  - RTT is stamped at parse time (`probe.rs:367`), so a late reply is read in the next round with RTT ≈ interval.
  - Final-round replies are never drained, which shows up as loss. The fixed 200 ms timeout (`:212`) breaks long paths.
  - Fix: see R1. *M*
- [x] **B4 `--report` without `-c` never finishes or prints** in real mode (`session.rs:137-142`; output is only written after `run_trace` returns). The README example hangs. Fix: default to 10 cycles, as mtr does. *S*
- [x] **B5 The build only works thanks to a lockfile that isn't committed.**
  - `socket2::Type::RAW` (`probe.rs:82`, `:88`) requires socket2's `all` feature, which only arrives through tokio 1.45's socket2 0.5.
  - With a newer tokio (socket2 0.6) the build fails with E0599, and `Cargo.lock` is gitignored.
  - Fix: `socket2 = { version = "0.5", features = ["all"] }`, and commit `Cargo.lock`. *S*

## P1: measurement correctness

- [x] **B6 Replies matched by sequence number only.**
  - There is no identifier, inner-protocol or inner-type check, and the inner header is assumed to start at a fixed `8 + 20` (`probe.rs:338-366`, v6 `:405-441`).
  - Raw sockets see all ICMP on the host, so other tools' traffic and a second mtr-ng instance (same start seq) get cross-attributed. An inner IHL > 5 loses the probe.
  - Fix: parse IHL, require the inner proto/type/id to match, and randomise the start seq. *S*
- [x] **B7 DNS uses Google's resolvers and blocks probing.**
  - `ResolverConfig::default()` (`session.rs:50-51`) is Google DNS. It ignores the system resolver and split-horizon DNS, and sends RFC 1918 PTR queries out of the network.
  - Report mode awaits an uncached reverse lookup inline for every reply (`:256`, `:296`, `:312`), which inflates RTTs.
  - Fix: `tokio_from_system_conf()`, a per-address cache, and lookups off the probe path. *S*
- [x] **B8 Timeouts never become loss, and replies land in the wrong slot.** *(m2: each probe carries its ordinal into HopStats; with real probes the TUI shows 0% on responsive hops instead of ~17%.)*
  - `Timeout` responses are ignored (`session.rs:325-328`, `:725-727`), so lost probes stay `?` forever.
  - `add_rtt` marks the newest Pending slot regardless of seq (`hop_stats.rs:283-288`).
  - In-flight probes count as lost (`:337-343`), so loss% oscillates every round.
  - Fix: carry the seq into HopStats; compute loss as (xmit − transit) / xmit, as mtr does. *M*
- [x] **B9 Multipath accounting is wrong.** *(m2: `ResponderStats` per address, primary = argmax. The split still measures random flows until F1.)*
  - `AlternatePath` starts at frequency 1 and `update()` adds 1 (`hop_stats.rs:25-43`), so an even A/B split shows 43/57.
  - On a primary switch the new primary stays listed as an alternate and the old one vanishes (`:122-160`).
  - Alternate replies never update `packet_history` and are left out of the RTT stats.
  - The hostname isn't reset on a switch, so B's IP is shown with A's PTR name.
  - Fix: one per-address map, with primary = argmax (see R3). *M*
- [x] **B10 `-P udp|tcp` silently send ICMP Echo** (`probe.rs:137-144`, `:153-160`); interactive mode ignores the flag (`session.rs:614`). Fix: reject the flag until it is implemented (X-series). *S*
- [x] **B11 One `send_to` error aborts the whole report** with no output (`probe.rs:169` → `session.rs:221`), e.g. ENETUNREACH during a Wi-Fi blip. Fix: count the probe as lost and continue. *S*
- [x] **B12 Avg/StDev/JAvg cover only the last 100 replies**, while Best/Worst/Loss are all-time (`hop_stats.rs:263-268`, `:310-311`). Fix: running Welford accumulators; keep the ring buffer for the graph only. *S*

## P2: visible defects

### TUI
- [x] **B13 A panic leaves the terminal broken.**
  - `ui/main.rs:320-441` has no panic hook or drop guard, and the `terminal.draw(..)?` early returns (`:389`, `:418`) skip the restore code.
  - Reproduced: exit code 101 left the terminal at `-icanon -isig -echo`, on the alternate screen with mouse capture on.
  - Panic sources:
    - Byte slicing of hostnames: `&hostname[..15]` (`ui/main.rs:196-199`, which also fires on short IPv6 alternate rows), `&hostname[..77]` (`ui/widgets.rs:530`), `truncate_hostname` (`utils.rs:281`). Hickory decodes IDN PTR names to Unicode, so this can happen with real names.
    - `total_sent - total_received` usize underflow (`ui/widgets.rs:177`).
  - Fix: `ratatui::init()`/`restore()`, `saturating_sub`, and truncation by display width with `unicode-width`. *S*
- [ ] **B14 The hostname column is starved.** Host and Graph are set to `Percentage(40/60)` on top of fixed `Length`s (`ui/widgets.rs:145`, `:162`). Host gets 5 characters from 80 to 120 columns and 0 at 50. Fix: `Min`/`Fill`, sized from the longest name (see U3). *S*
- [ ] **B15 The graph hides the newest samples.** The graph width is guessed (minimum 20, `ui/widgets.rs:537-565`) instead of read from the column, so a narrow column clips the most recent samples and a wide one leaves blanks. Fix: derive it from the real `Rect` (U3). *S*
- [ ] **B16 The log-scale sparkline goes flat on LANs.**
  - RTTs are truncated to whole ms (`utils.rs:13-15`, `:214`; `ui/main.rs:107-116`), so a 0.4 ms gateway makes the minimum 0. Every bar on every hop then renders ▁.
  - With all RTTs < 1 ms the scale says "No RTT data available" (`ui/widgets.rs:405`).
  - The labels use `ln(x+1)` while the bars use `ln(x)` (`ui/widgets.rs:431-436`).
  - Fix: f64 µs, and `log(x+ε)` everywhere. *S*
- [ ] **B17 Key handling.** *(Ctrl-C quits since m1; the rest is open.)*
  - Ctrl-C cycles the colour mode instead of quitting (`ui/events.rs:88-106`, `:103`).
  - Esc in the help overlay quits the app (`:89`).
  - The help popup is hard-coded to 12 rows (`ui/main.rs:267-269`), cutting off content, and `?` isn't listed in it.
  - Help and the column selector can be open at once.
  - Shift+↑/↓ reorder is dead code (`ui/events.rs:36-41` vs `:58-75`).
  - `f` replaces Host with Jitter (`ui/state.rs:142-159`).
  - `r` resets `--ema-alpha` to 0.1 (`ui/events.rs:144`).
  - Fix: see U2. *S*
- [ ] **B18 The column selector loses order.** It is rebuilt in canonical order every time it opens (`ui/state.rs:55-63`, `ui/widgets.rs:29-40`), discarding both a custom order and the `--fields` order. It has no scrolling and cuts rows below 24 lines. *S*
- [ ] **B19 Hop list and status bar.** *(Phantom rows and status totals fixed in m1; scrolling is open.)*
  - Phantom `???` rows past the target.
  - The status bar aggregates over all `max_hops` ("Hops: 30", "Loss: 76%").
  - No table scrolling for long paths.
  - The status-bar Loss averages over silent routers too: two hops that never answer TTL-expired probes give "Loss: 28.6%" while every responsive hop and the target are at 0%. Show the target's loss instead (mtr has no aggregate).
  - Fix: follow B2, and compute status over hops up to the target. *S*
- [ ] **B20 Lifecycle.** *(Resize redraws and trace errors surface since m1; R1 drains the last round. The done state is open.)*
  - Resize is ignored, because redraws only happen on key events (`ui/main.rs:396-425`).
  - There is no "done" state after `-c N`, and the last round's replies are dropped (`session.rs:695-698`).
  - Trace-task errors only reach the log (`ui/main.rs:349-353`). *S*
- [ ] **B21 Truncated headers and mixed units.**
  - Headers 7-8 characters wide are drawn in `Length(5/6)` columns (`ui/main.rs:133`), giving "Los" and "WorstR".
  - "100.0%" loses its %.
  - Last switches to µs while Avg/Best stay in ms, with no unit shown (`ui/widgets.rs:113-127`). *S*
- [ ] **B22 Colour and terminal etiquette.** *(Mouse capture removed in m1.)*
  - `NO_COLOR` and `TERM=dumb` are ignored (`ui/visualization.rs:344-365`).
  - Explicit `Color::White` is invisible on light themes (`ui/widgets.rs:470`).
  - Unused `EnableMouseCapture` (`ui/main.rs:323`) blocks text selection for copying IPs.
  - The status bar says "Hostnames" under `-n` (`ui/widgets.rs:193`). *S*

### CLI, report, docs
- [ ] **B23 The CLI accepts nonsense:** `--ema-alpha NaN|0|-1`, `-i 0` (a tight send loop), `-c 0`, `-M 0` (`args.rs`). Fix: clap `value_parser` ranges. *S*
- [ ] **B24 Flags that do nothing:** `--timing` and `-q` (`args.rs:177-183`); `-q` still logs to stderr. *S*
- [ ] **B25 The man page contradicts the code.**
  - `-i` is documented in seconds; it's ms, so `-i 1` means 1 ms.
  - `-f` doesn't exist.
  - The sparkline default is wrong.
  - `-M`, `-P`, `--simulate` and `--ema-alpha` are missing.
  - The `-c` help says "pings per round"; it's rounds.
  - Fix: generate the man page with `clap_mangen`. *S*
- [ ] **B26 Report formatting.** *(m2: widths come from the data, the host column fits the longest name as `mtr -w` does, `???` lines up, `--fields` works; golden tests in `report.rs`. `HOST: localhost` is open.)*
  - Headers drift right of the values (`report.rs:15-23` vs `:44-96`).
  - `???` is narrower than the values.
  - A fixed 29-space prefix breaks with `--fields`.
  - Long PTR names aren't truncated.
  - `HOST: localhost` is hard-coded. *S*
- [x] **B27 Logging and version.**
  - Interactive mode appends DEBUG logs to `./mtr-ng.log` in the CWD, with the level hard-coded (so `RUST_LOG` is ignored) and no size limit. An 809 MB root-owned file exists in this checkout.
  - `main.rs:17` and `:35` hard-code `v0.1.0`.
  - Fix: log only with `--log-file`, take the level from `RUST_LOG`, write to a state directory; use `env!("CARGO_PKG_VERSION")`. *S*

## P3: cleanup

- [ ] **B28 Polling disguised as event-driven I/O.** *(R1 removed the probe half. The UI half waits for U1.)*
  - Each receive dups the fd and registers/deregisters it with the reactor (~1000×/s in interactive mode; `probe.rs:201-209`).
  - ICMPv6 replies are matched against the IPv4 pending map (`:231-261`).
  - A blocking `crossterm::event::poll` runs on a tokio worker (`ui/main.rs:360-376`).
  - Every update triggers a full redraw through an unbounded channel. Superseded by R1 and U1. *M*
- [x] **B29 Dead or misleading state.** *(m1 removed `pnet` and `collect_responses`; m2 removed the sequence table, `TimingStats`, and 21 dead helpers in `utils.rs`, including a second ICMP packet builder.)*
  - `sequence_table` is never pruned (~32k entries).
  - `next_sequence` isn't the seq that goes on the wire.
  - `packet_id` and `batch_at` are unused.
  - `TimingStats` variance is wrong and unused (`utils.rs:179-185`).
  - The `pnet` dependency is unused.
  - `collect_responses` uses `block_in_place`. *S*
- [x] **B30 Simulation artefacts.** There are phantom rows 9-10 because `break` only fires on success (`session.rs:436`), and the realtime simulation probes all 30 hops. *S*
- [x] **B31 CI is red:** 130 `cargo fmt` hunks across 11 files, and 9 clippy warnings under `-D warnings`. *S*

## Foundations

These refactors unblock most fixes above and every feature below.

- [x] **R1 One probing pipeline.** *(m2, `tracer.rs`. It uses a blocking receive thread per address family instead of `AsyncFd`, which also works on Windows, and timestamps in user space on arrival instead of `SO_TIMESTAMP`; that costs microseconds and can come later.)* Replace the two divergent loops (report: batch + 50 ms window + 200 ms timeout; interactive: channels + 5 s timeout) with:
  - one long-lived receiver task per address family on an `AsyncFd`;
  - kernel receive timestamps (`SO_TIMESTAMP`);
  - a single timeout/loss policy.
  - Fixes B3, B8, B11, B28. *L*
- [x] **R2 Testable seams.** *(m2: `Prober` trait, `FakeProber` in `tracer.rs` tests, pure `decode`/`parse_icmpv4`. Simulation is not yet a `Prober`: it still has its own loops in `session.rs`.)* A `Prober` trait (real, simulated, fake) and pure `parse(&[u8]) -> Option<Reply>` functions tested against captured packets. Session logic (hop trimming, loss, end of path) is then tested against the fake. *M*
- [ ] **R3 Per-probe records as the source of truth.** *(m2: `ResponderStats` replaced `AlternatePath`. Per-probe records are open.)* `ProbeSpec { ttl, flow, proto, size, df }` and a ring of `ProbeRecord`s (responder, RTT, reply TTL, quoted header, ICMP extensions). `HopStats` and a new per-responder `PerAddrStats` derive from it, replacing `AlternatePath`. Fixes B9 and B12; needed for F1, F2 and F7. *M*
- [ ] **R4 Tests.** *(m2: golden report tests, multipath and stale-ordinal HopStats tests. `TestBackend` render tests are open.)*
  - Golden tests for report output.
  - Table-driven multipath and out-of-order tests for HopStats.
  - `TestBackend` render tests at several sizes.
  - Size: *S* each.
- [ ] **U1 Snapshot render loop.**
  - Publish a `PathSnapshot` through `tokio::sync::watch` and render it without holding the session lock.
  - Drive redraws from input, a coalesced dirty flag, a tick and resize, using crossterm's `EventStream`.
  - Fixes B20 and B28 (UI half). *M*
- [ ] **U2 View components and modes.**
  - A `View` trait (`render(area, &snapshot)`, `handle_key -> Action`).
  - A modal/focus stack in place of the boolean flags.
  - One keymap table that also generates the help text.
  - Fixes B17. *M*
- [ ] **U3 One `ColumnSpec` table** (id, header, label, constraint, alignment, cell function) that replaces the seven parallel `match Column` sites. `Fill`/`Min` layout; graph width from the real `Rect`. Fixes B14, B15, B18 and B21. *M*

## Cross-platform

| Platform | Status today | Main blockers |
|---|---|---|
| Linux (glibc/musl, x86_64/aarch64) | Works with root/CAP_NET_RAW | B1, B2, B5, B10; IPv6 simulated |
| macOS | Works with sudo | Same; **unprivileged `SOCK_DGRAM` ICMP verified live on Darwin 25.6** (Time Exceeded replies arrive with headers intact; the existing parser works unchanged) |
| Windows | Compiles, broken at runtime | Needs admin; raw socket never `bind()`-ed (plausible WSAEINVAL); keys fire twice; `/dev/null` fallback panics |
| FreeBSD | Type-checks (`x86_64-unknown-freebsd`); untested | No CI, no artefact, `install.sh` rejects it; no unprivileged ICMP on FreeBSD |

- [x] **X1 Windows defects.**
  - Every key fires twice, because Release events aren't filtered (`ui/main.rs:397`; crossterm emits them on Windows). Filter on `KeyEventKind::Press`.
  - `File::create("/dev/null")` fallback panics (`main.rs:27`). Use `io::sink()`.
  - Npcap is only needed because of the unused `pnet` (see C2). *S*
- [x] **X2 Spike: adopt `trippy-core` behind `Prober`?** **Verdict: no.** It reports per round rather than per probe, timestamps with `SystemTime`, can't do ICMP Paris, and its engine isn't pluggable. Consider `trippy-packet` (Apache-2.0) for F1/F9 packet work. Side finding: `IP_HDRINCL` works on macOS datagram ICMP sockets.
  - Original question: It covers ICMP/UDP/TCP, IPv4/IPv6, Paris/Dublin and five OSes, but its unprivileged mode only works on macOS. It has a 0.x API, its round model differs from HopStats, and its last release was May 2025. Decide before investing in R1. *S*
- [ ] **X3 `Prober` backends** (on R2). Each runs its own blocking receive thread that timestamps on arrival and feeds a channel, which sidesteps Unix-only `AsyncFd`. `select_prober()` picks a backend per OS and privilege level and says why.
  - **a. Raw ICMP** (root/admin, all OSes): the current code, plus id matching, IHL parsing, the IPv6 hop limit (`set_unicast_hops_v6`, not `set_ttl` at `probe.rs:150`), `ICMP6_FILTER`, and a Windows `bind`. *S-M*
  - [x] **b. macOS unprivileged ICMP:** `Type::DGRAM` with the same parser. Make it the macOS default so Homebrew users never need sudo. *S*
  - **c. Linux unprivileged ICMP:** a ping socket plus `IP_RECVERR`, reading `MSG_ERRQUEUE`. Needs `ping_group_range` (open by default on systemd ≥ 243). The kernel rewrites the echo id, so match on seq. *M*
  - **d. Linux unprivileged UDP:** tracepath-style with `IP_RECVERR`, no sysctl needed; a real `-P udp`. *M*
  - **e. Windows unprivileged ICMP:** `IcmpSendEcho2Ex`/`Icmp6SendEcho2` via `windows-sys`, as tracert and WinMTR do. Time it with `Instant` (the API reports whole ms). *M*
  - **f. TCP SYN:** defer. *L*
- [ ] **X4 Privileges.**
  - Linux: a deb postinst `setcap`, and drop capabilities after opening sockets (`caps` crate), as Debian's mtr does.
  - FreeBSD: setuid-root with an immediate privilege drop, or document `doas`.
  - Not needed on macOS/Windows once b/e exist. *S-M*

## Build, CI and distribution

Every install path is broken today, and CI is red.

- [x] **C1 Reproducible builds.** B5, plus `--locked` in CI, release and every documented install command.
- [x] **C2 Drop unused dependencies:** `pnet`, `libc`, `nix`, `tokio-util`, `serde`, `serde_json`, `thiserror`, `tokio-test` (re-add `nix`/`libc` when X3c needs them). Dropping `pnet` removes both Npcap CI steps (`ci.yml:72-84`, `release.yml:163-180`). `unicode-width` is also unused, but keep it for B13. *S*
- [ ] **C3 CI.**
  - The lint gate is red: B31 (130 fmt hunks over 11 files). Pin the toolchain for the lint job.
  - The cache key hashes the untracked lockfile, so it never refreshes (`ci.yml:26-32`). Switch to `Swatinem/rust-cache` and `checkout@v5` (the Node 20 actions are deprecated).
  - The tag/version check (`ci.yml:40-57`) never runs, because the workflow isn't triggered by tags.
  - Target matrix: ubuntu x64 + arm, macos-15, windows; `cargo check` for FreeBSD; an MSRV job; a privileged loopback smoke test (`-r -c 2 -n 127.0.0.1`).
  - Size: *S-M*
- [ ] **C4 Release pipeline.**
  - `release.yml:109-113` uses `macos-13`, which was retired 2025-12-04.
  - `workflow_dispatch` derives the version from the branch ref, so the verify step fails.
  - The release is published before the builds run.
  - No checksums are uploaded.
  - Archives lack LICENSE and the man page.
  - Permissions are broader than needed.
  - `cargo publish` fails on B5.
  - Recommendation: **cargo-dist**, which produces archives, checksums, shell/PowerShell installers and the Homebrew tap formula, replacing `install.sh`, `Formula/` and most of `release.yml`. Use release-plz or cargo-release instead of `bump-version.sh`, which only edits `Cargo.toml` and waits forever on skipped or timed-out CI. *M*
- [ ] **C5 Broken install paths.** *(m1: one-liner removed from README and release notes, `curl -f` in `install.sh`, README lists only working routes. The rest waits for C4.)*
  - `install.sh` downloads from `releases/download/latest/mtr-ng-$os-$arch`, which matches neither GitHub's URL scheme nor the asset names. `curl -L` without `-f` saves the 404 page, which is then installed **with sudo as the binary** (`install/install.sh:177`, `:183`, `:191`). It has no checksum check and uses sudo even for `~/.local`.
  - The README one-liner points at `/main/`, but the branch is `master`, so it pipes "404: Not Found" into bash (`README.md:43`, `release.yml:62`, `install.sh:194`).
  - The published tap formula has no `install` block or sha256. The in-repo formula's test expects text `--help` doesn't print (`Formula/mtr-ng.rb:19`).
  - `packaging/PKGBUILD`: pkgver 0.1.0, `sha256sums=SKIP`, `--locked` without a lockfile, and an invalid triple on armv7h. The README's `yay -S mtr-ng` doesn't exist on the AUR.
  - Fix: pull the one-liner from the README now; replace the rest via C4. *S*
- [ ] **C6 `setup-capabilities.sh`.**
  - It checks for `cap_net_raw+ep`, but libcap ≥ 2.41 prints `=ep`, so it exits 1 after succeeding (`:104`).
  - It sets the capability on `target/release` (`:84`, `:89`), so every rebuild wipes it.
  - It rejects musl.
  - It asks for confirmation before checking the platform.
  - Its warning that file capabilities are "less secure than sudo" has it backwards. *S*
- [ ] **C7 Licence mismatch.** `Cargo.toml` says MIT OR Apache-2.0, but only MIT text ships; the README says MIT; the PKGBUILD says Apache. Pick one and align. *S*
- [ ] **C8 Dependencies.**
  - Advisories reported (via OSV) against hickory-proto 0.24, `lru` (via ratatui 0.29) and the unmaintained `paste`; upgrading to hickory 0.25+ and ratatui 0.30 + crossterm 0.29 clears them. The local `cargo-audit` is too old to read the current database, so re-check with a fresh `cargo-deny`.
  - Edition 2024 compiles cleanly.
  - Declare `rust-version`. The real minimum is 1.82 with the current lockfile and 1.88 on a fresh resolve; DISTRIBUTION.md claims 1.70.
  - Add `clap_mangen` (B25) and `clap_complete`. *S-M*
- [ ] **C9 Repo hygiene.**
  - Delete the tracked `test_output.txt`/`test_results.txt`.
  - Delete the stray logs in the root. `mtr-ng.log` and `mtr-ng-su.log` are root-owned (`sudo rm`); `sixel_debug.log` predates the sixel removal.
  - `just compare` runs `sudo cargo run`, which creates root-owned build output. `just report` writes an un-ignored file.
  - `Cargo.toml` uses `exclude`, so `.cursor/`, `Formula/`, `packaging/` and `scripts/` ship in the crate; use `include`.
  - `.cursor/*.md` is stale (the changelog stops at 0.1.0): fold it into DEVELOPMENT.md or delete it.
  - README:
    - lines 229-263 are a pasted tap README;
    - it claims Windows works.
  - `packaging/debian/control` and the `generate-rpm` metadata are dead. *S*

## Features

The current "multipath" view has a structural flaw. ICMP probes change their checksum with every sequence number (`construct_icmp_packet`), and ECMP load balancers hash on it. Every probe is therefore a new flow, and the alternate-path percentages measure how random flows spread, not distinct routes. Stable flow identifiers (F1) are the keystone for every multi-route feature.

| ID | Feature | Design notes | Needs | Effort |
|---|---|---|---|---|
| F1 | Paris-style stable flows | ICMP: adjust two payload bytes per seq so the checksum stays constant (scamper's ICMP-Paris; also works on unprivileged datagram sockets). UDP/TCP: fixed ports, probe id in the checksum or TCP seq. `--flows N`. | R1, R3 | M |
| F2 | Multi-route model and views | `topology.rs` `PathGraph`: nodes (ttl, addr), edges from consecutive TTLs within one flow. Views: corrected per-hop alternates; a flow matrix (TTL × flow, one letter per responder, coloured by RTT/loss, so diamonds are visible at a glance); a layered DAG in box-drawing characters. DOT/Mermaid export. | F1, R3, U2 | M-L |
| F3 | MDA-Lite branch discovery | Enumerate ECMP branches with stated confidence, then probe one flow per branch continuously so each branch gets its own loss/RTT. Periodic rediscovery. | F1, F2 | L |
| F4 | NAT detection (Dublin) | Probe id in the IP ID with DF set; compare the quoted UDP checksum/ports with what was sent; mark `[NAT]`. IPv4 only in practice. | raw UDP | M |
| F5 | ASN and prefix | Team Cymru TXT via hickory `txt_lookup` (`origin.asn.cymru.com`, `AS<n>.asn.cymru.com`), cached per prefix. Offline: iptoasn.com (PDDL). Quick win. | none | S |
| F6 | Geolocation | `geo/` module; `GeoSource` trait → `GeoFix { lat, lon, accuracy_km, source }`; `maxminddb` crate (ISC). DB-IP City Lite (CC BY 4.0) downloaded on request; `--geoip-mmdb <path>` for GeoLite2 (never bundle it: EULA). RIPE IPmap opt-in (good on router IPs). Hostname hints (IATA/LOCODE/CLLI codes via OurAirports, public domain). | F12 | M |
| F7 | Distance and plausibility | Origin from `--origin lat,lon` or the first public hop. A hop is at most minRTT × ~100 km away (light in fibre, round trip). Flag geolocations that violate the bound (catches anycast and registrant-address errors). Columns: Dist, Reach, Stretch; plus path inflation vs the great-circle distance to the target. | F6, R3 | S-M |
| F8 | World map | `ui/map.rs`: `Canvas` + `Map { resolution: High }` + braille. Great-circle segments in ~2° steps, split at ±180°, **clipped manually** (ratatui 0.29 `canvas::Line` drops segments with an endpoint out of bounds). Auto-zoom with cos(lat) aspect. Unknown hops bridged as "+N hops"; impossible fixes drawn hollow red. Falls back to the table below ~60×20. | F6, U1, U2; better with F2, F7 | M |
| F9 | ICMP extensions | Real quoted-header length and the RFC 4884 length field; decode MPLS label stacks (RFC 4950) and interface info (RFC 5837). Needs pcap fixtures. | R2 | M |
| F10 | Path MTU and black-hole detection | DF + per-hop size search; parse Frag Needed / Packet Too Big. Neither mtr nor trippy has it. | R1 | M |
| F11 | Outputs | JSON/CSV via a serde transfer type (`HopStats` holds `Instant`s); OpenMetrics exporter for long-running monitoring; NDJSON event log with replay in the TUI. | none (replay: R3) | S-M |
| F12 | Privacy defaults | Offline lookups by default, online sources opt-in; never look up private/CGNAT/ULA/link-local addresses; `--privacy-hops N`; About pane with data licences. Ships with F5/F6. | none | S |
| F13 | Return-path hint | Estimate the reverse hop count from the reply TTL to flag asymmetric routing. | R3 | S |
| F14 | Per-hop detail pane | Selected row (`TableState`) opens per-responder stats, the RTT distribution, ASN/geo and MPLS labels. Needs a history longer than 100 samples. | U2, R3 | S-M |

**Positioning versus trippy.** Trippy already has broad protocol coverage, ASN/GeoIP, a world map with pins, and many output formats, so matching its breadth is a losing race. mtr-ng's opening is the *truthfulness of the path*:
- stable flows by default for ICMP and TCP too (trippy's Paris/Dublin are UDP-only);
- branches discovered deliberately with MDA and measured per branch;
- diamonds drawn as diamonds;
- geolocation treated as a hypothesis checked against the speed of light;
- a map that draws the path rather than pins.

PMTU/black-hole detection and an OpenMetrics exporter are further gaps in trippy.

## Suggested milestones

1. **Tell the truth and unbreak.** *(Done on branch `m1-tell-the-truth`, plus B6 and X3b pulled forward.)* B1, B2, B4, B5/C1, B10, B13, B27, B31, C2, C5 (at least pull the 404 one-liner), X1. Refuse rather than fabricate; never break the user's terminal; make it installable.
2. **Measure correctly.** *(Done: X2, R1, R2, B3, B7-B9, B11, B12, B29, X3b; R3/R4 partly.)* X2 spike first (it decides how much of R1 is written by hand), then R1, R2, R4, B3, B6-B9, B11, B12, X3b (macOS without sudo).
3. **A UI that can grow.** U1-U3, after which B14-B26 mostly fall out. Add F5 and F11 (JSON/CSV) as cheap wins. C3, C4 (cargo-dist), C6-C9.
4. **Stable flows and multi-route.** R3, F1, F2, F9, F14.
5. **Where is it.** F12, F6, F7, F8.
6. **Platforms and depth.** X3a/c/d/e, X4, IPv6, F4, F3, F10, F13.
