//! `--record PATH` writes what a trace sees as it happens, one JSON object per
//! line (NDJSON); `--replay PATH` plays such a log back into the TUI or a
//! report, without sending a probe or a lookup.
//!
//! The log holds what the session was told, not the packets: each probe sent,
//! each reply (from whom, how long it took, its TTL and MPLS labels) or timeout,
//! and each name and AS that arrived. Played back in order, the same events
//! build the same statistics. The path MTU search and `--flows auto`'s branch
//! search are not recorded: a replay shows the flows, not their notes.
//!
//! Hops hidden by `--privacy-hops` are recorded with stand-ins (see `privacy`),
//! so a log can be passed on like a report.

use crate::args::{Args, Flows, ProbeProtocol};
use crate::asn::AsInfo;
use crate::icmp_ext::MplsLabel;
use crate::privacy::{self, StandIns};
use crate::probe::{IcmpResponseType, ProbeResponse, MAX_FLOWS};
use crate::session::MtrSession;
use crate::Result;
use anyhow::{bail, Context};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tracing::warn;

/// The first line of a log: what was traced, from where and how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Start {
    /// The mtr-ng version that wrote the log
    pub version: String,
    /// When the trace started, RFC 3339
    pub start: String,
    /// The probing host's name, or "(hidden)" with --privacy-hops
    pub source: String,
    pub target: String,
    pub target_addr: IpAddr,
    pub protocol: String,
    /// `8` or `auto`
    pub flows: String,
    pub interval_ms: u64,
    pub max_hops: u8,
    #[serde(default)]
    pub simulated: bool,
}

/// What kind of reply came back
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    EchoReply,
    TimeExceeded,
    PortUnreachable,
    Unreachable,
}

impl From<Answer> for IcmpResponseType {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::EchoReply => Self::EchoReply,
            Answer::TimeExceeded => Self::TimeExceeded,
            Answer::PortUnreachable => Self::PortUnreachable,
            Answer::Unreachable => Self::DestinationUnreachable,
        }
    }
}

/// One line of the log after the first. Hops count from 1, like TTLs; `probe`
/// is the probe's number at its hop, from 0, as `HopStats::increment_sent`
/// hands them out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Start(Start),
    Sent {
        hop: u8,
        flow: u16,
        probe: u64,
    },
    Reply {
        hop: u8,
        probe: u64,
        from: IpAddr,
        kind: Answer,
        rtt_ms: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_ttl: Option<u8>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mpls: Vec<MplsLabel>,
    },
    Timeout {
        hop: u8,
        probe: u64,
    },
    /// A reverse lookup's answer; no name if there was none
    Name {
        addr: IpAddr,
        name: Option<String>,
    },
    /// An origin AS lookup's answer
    Asn {
        addr: IpAddr,
        asn: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    AsName {
        asn: u32,
        name: Option<String>,
    },
    /// The statistics were started over (`r` in the TUI)
    Reset,
}

/// A line: seconds since the trace started, and what happened.
#[derive(Debug, Serialize, Deserialize)]
struct Line {
    t: f64,
    #[serde(flatten)]
    event: Event,
}

/// Writes the log. Shared by the session's copies; only the one the tracer
/// drives writes to it.
pub struct Recorder {
    began: Instant,
    privacy_hops: u8,
    state: Mutex<Writer>,
}

struct Writer {
    out: Option<BufWriter<File>>,
    stand_ins: StandIns,
}

impl Recorder {
    /// Create `path`, which must not exist, and write the first line.
    pub fn create(path: &Path, start: Start, privacy_hops: u8) -> Result<Arc<Self>> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("--record: cannot create {}", path.display()))?;
        let recorder = Self {
            began: Instant::now(),
            privacy_hops,
            state: Mutex::new(Writer {
                out: Some(BufWriter::new(file)),
                stand_ins: StandIns::default(),
            }),
        };
        recorder.write(Event::Start(start));
        Ok(Arc::new(recorder))
    }

    /// Write `event`. After a failed write the log stops, with one warning.
    fn write(&self, event: Event) {
        let flush = matches!(
            event,
            Event::Start(_) | Event::Sent { hop: 1, .. } | Event::Reset
        );
        let t = match event {
            Event::Start(_) => 0.0,
            _ => self.began.elapsed().as_micros() as f64 / 1e6,
        };
        let mut state = self.state.lock().unwrap();
        let Some(out) = state.out.as_mut() else {
            return;
        };
        let written = serde_json::to_writer(&mut *out, &Line { t, event })
            .map_err(std::io::Error::from)
            .and_then(|()| out.write_all(b"\n"))
            // Once a round, so a killed trace loses little
            .and_then(|()| if flush { out.flush() } else { Ok(()) });
        if let Err(e) = written {
            warn!("--record: cannot write, the log stops here: {e}");
            state.out = None;
        }
    }

    /// Write out what is buffered.
    pub fn flush(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(Err(e)) = state.out.as_mut().map(Write::flush) {
            warn!("--record: cannot write, the log stops here: {e}");
            state.out = None;
        }
    }

    /// `addr` as the log shows it: a stand-in if it answered at a hidden hop.
    fn show(&self, hop: usize, addr: IpAddr) -> IpAddr {
        let mut state = self.state.lock().unwrap();
        if hop <= usize::from(self.privacy_hops) {
            state.stand_ins.get(addr)
        } else {
            state.stand_ins.replace(addr)
        }
    }

    /// True for an address that was hidden somewhere: its lookups stay out.
    fn hidden(&self, addr: IpAddr) -> bool {
        let state = self.state.lock().unwrap();
        privacy::is_stand_in(state.stand_ins.replace(addr))
    }

    pub fn sent(&self, hop: usize, flow: u16, probe: u64) {
        let hop = (hop + 1) as u8;
        self.write(Event::Sent { hop, flow, probe });
    }

    pub fn timeout(&self, hop: usize, probe: u64) {
        let hop = (hop + 1) as u8;
        self.write(Event::Timeout { hop, probe });
    }

    pub fn response(&self, response: &ProbeResponse) {
        let Some(probe) = response.ordinal else {
            return;
        };
        let hop = response.hop + 1;
        let kind = match response.icmp_type {
            IcmpResponseType::EchoReply => Answer::EchoReply,
            IcmpResponseType::TimeExceeded => Answer::TimeExceeded,
            IcmpResponseType::PortUnreachable => Answer::PortUnreachable,
            IcmpResponseType::DestinationUnreachable => Answer::Unreachable,
            IcmpResponseType::Timeout => return self.timeout(response.hop, probe),
            IcmpResponseType::TooBig { .. } => return,
        };
        self.write(Event::Reply {
            hop: hop as u8,
            probe,
            from: self.show(hop, response.source_addr),
            kind,
            rtt_ms: response.rtt.as_nanos() as f64 / 1e6,
            reply_ttl: response.reply_ttl,
            mpls: response.mpls.clone(),
        });
    }

    pub fn name(&self, addr: IpAddr, name: &Option<String>) {
        if !self.hidden(addr) {
            let name = name.clone();
            self.write(Event::Name { addr, name });
        }
    }

    pub fn asn(&self, addr: IpAddr, asn: &Option<AsInfo>) {
        if !self.hidden(addr) {
            self.write(Event::Asn {
                addr,
                asn: asn.as_ref().map(|info| info.number),
                prefix: asn.as_ref().and_then(|info| info.prefix.clone()),
            });
        }
    }

    pub fn as_name(&self, asn: u32, name: &Option<String>) {
        let name = name.clone();
        self.write(Event::AsName { asn, name });
    }

    pub fn reset(&self) {
        self.write(Event::Reset);
    }
}

/// The first line for a trace about to start.
pub fn start(args: &Args, target: &str, target_addr: IpAddr) -> Start {
    let source = match args.privacy_hops {
        0 => gethostname::gethostname().to_string_lossy().into_owned(),
        _ => privacy::HIDDEN.to_string(),
    };
    let protocol = args.protocol.to_possible_value();
    Start {
        version: env!("CARGO_PKG_VERSION").to_string(),
        start: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        source,
        target: target.to_string(),
        target_addr,
        protocol: protocol.map_or_else(String::new, |p| p.get_name().to_string()),
        flows: match args.flows {
            Flows::Fixed(n) => n.to_string(),
            Flows::Auto => "auto".to_string(),
        },
        interval_ms: args.interval,
        max_hops: args.max_hops,
        simulated: args.simulate,
    }
}

/// A log being played back.
pub struct Replay {
    path: PathBuf,
    pub start: Start,
    lines: Lines<BufReader<tokio::fs::File>>,
}

impl Replay {
    /// Open `path` and read its first line.
    pub async fn open(path: &Path) -> Result<Self> {
        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("--replay: cannot open {}", path.display()))?;
        let mut lines = BufReader::new(file).lines();
        let first = lines.next_line().await?.unwrap_or_default();
        let Ok(Line {
            event: Event::Start(start),
            ..
        }) = serde_json::from_str(&first)
        else {
            bail!("{} is not a log from mtr-ng --record", path.display());
        };
        Ok(Self {
            path: path.to_path_buf(),
            start,
            lines,
        })
    }

    /// The session the log was recorded in, with the display options of `args`.
    pub async fn session(&self, mut args: Args) -> Result<MtrSession> {
        let start = &self.start;
        args.target = start.target_addr.to_string(); // no lookup
        args.max_hops = start.max_hops;
        args.interval = start.interval_ms;
        args.flows = match start.flows.parse() {
            Ok(n) => Flows::Fixed(n),
            Err(_) => Flows::Fixed(1), // auto: as many as the log turns out to use
        };
        args.protocol = ProbeProtocol::from_str(&start.protocol, true).unwrap_or(args.protocol);
        args.simulate = start.simulated;
        let mut session = MtrSession::new(args).await?;
        session.target = start.target.clone();
        session.replayed = Some(start.clone());
        Ok(session)
    }

    /// Play the log into `session`: `speed` times as fast as it was recorded, or
    /// all at once without one.
    pub async fn run(mut self, session: Arc<Mutex<MtrSession>>, speed: Option<f64>) -> Result<()> {
        let began = Instant::now();
        let mut skipped = 0;
        while let Some(line) = self.lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            // A trace that was killed can leave half a line
            let Ok(Line { t, event }) = serde_json::from_str(&line) else {
                skipped += 1;
                continue;
            };
            if let Some(speed) = speed {
                let due = Duration::try_from_secs_f64(t / speed).unwrap_or_default();
                if began.elapsed() < due {
                    session.lock().unwrap().notify();
                    tokio::time::sleep_until((began + due).into()).await;
                }
            }
            session.lock().unwrap().replay(event);
        }
        session.lock().unwrap().notify();
        if skipped > 0 {
            warn!(
                "{}: skipped {skipped} lines that aren't events",
                self.path.display()
            );
        }
        Ok(())
    }
}

/// The flows a replay shows, now that `flow` turned up.
pub fn flows_with(flows: Flows, flow: u16) -> Flows {
    match flows {
        Flows::Fixed(n) if flow < MAX_FLOWS => Flows::Fixed(n.max(flow + 1)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A log path of its own for each test, removed first
    fn log_path(name: &str) -> PathBuf {
        let file = format!("mtr-ng-{}-{name}.ndjson", std::process::id());
        let path = std::env::temp_dir().join(file);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn reply(hop: usize, probe: u64, from: IpAddr) -> ProbeResponse {
        let now = Instant::now();
        ProbeResponse {
            hop,
            ordinal: Some(probe),
            seq: 0,
            source_addr: from,
            icmp_type: IcmpResponseType::TimeExceeded,
            rtt: Duration::from_millis(5),
            send_time: now,
            receive_time: now,
            precise_rtt_ns: 5_000_000,
            reply_ttl: Some(62),
            mpls: Vec::new(),
        }
    }

    async fn replay(path: &Path) -> MtrSession {
        let replay = Replay::open(path).await.unwrap();
        let args = Args::parse_from(["mtr-ng", "--replay", path.to_str().unwrap()]);
        let session = Arc::new(Mutex::new(replay.session(args).await.unwrap()));
        replay.run(Arc::clone(&session), None).await.unwrap();
        let session = session.lock().unwrap().clone();
        session
    }

    #[test]
    fn events_are_one_json_object_a_line() {
        let line = Line {
            t: 1.25,
            event: Event::Reply {
                hop: 3,
                probe: 7,
                from: "198.51.100.1".parse().unwrap(),
                kind: Answer::TimeExceeded,
                rtt_ms: 12.345678,
                reply_ttl: Some(61),
                mpls: Vec::new(),
            },
        };
        let text = serde_json::to_string(&line).unwrap();
        assert_eq!(
            text,
            r#"{"t":1.25,"event":"reply","hop":3,"probe":7,"from":"198.51.100.1","kind":"time_exceeded","rtt_ms":12.345678,"reply_ttl":61}"#
        );
        let back: Line = serde_json::from_str(&text).unwrap();
        assert_eq!(back.event, line.event);
        let reset: Line = serde_json::from_str(r#"{"t":3,"event":"reset"}"#).unwrap();
        assert_eq!(reset.event, Event::Reset);
    }

    #[tokio::test]
    async fn names_and_ases_are_replayed_and_hidden_hops_stay_hidden() {
        let path = log_path("names");
        let gateway: IpAddr = "192.168.1.1".parse().unwrap();
        let core: IpAddr = "198.51.100.1".parse().unwrap();
        let cli = ["mtr-ng", "-n", "--privacy-hops", "1", "--record"];
        let cli = cli.into_iter().chain([path.to_str().unwrap(), "192.0.2.1"]);
        let mut live = MtrSession::new(Args::parse_from(cli)).await.unwrap();
        for (hop, from) in [(0, gateway), (1, core)] {
            let probe = live.sent(hop, 0);
            live.apply_response(&reply(hop, probe, from));
        }
        let probe = live.sent(1, 0);
        live.lost(1, probe);
        live.set_hostname(gateway, Some("gateway.home".to_string()));
        live.set_hostname(core, Some("core-1.example".to_string()));
        let info = AsInfo {
            number: 64500,
            prefix: Some("198.51.100.0/24".to_string()),
            name: None,
        };
        live.set_asn(core, Some(info));
        live.set_as_name(64500, Some("EXAMPLE".to_string()));
        live.recorder.as_ref().unwrap().flush();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("192.168.1.1"), "{text}");
        assert!(!text.contains("gateway.home"), "{text}");
        let s = replay(&path).await;
        std::fs::remove_file(&path).unwrap();

        assert_eq!(s.replayed.as_ref().unwrap().source, privacy::HIDDEN);
        assert!(privacy::is_stand_in(s.hops[0].addr.unwrap()));
        let hop2 = &s.hops[1];
        assert_eq!((hop2.sent, hop2.received), (2, 1));
        assert_eq!(hop2.loss_percent, 50.0);
        assert_eq!(hop2.hostname.as_deref(), Some("core-1.example"));
        let asn = hop2.asn.as_ref().unwrap();
        assert_eq!(asn.number, 64500);
        assert_eq!(asn.prefix.as_deref(), Some("198.51.100.0/24"));
        assert_eq!(asn.name.as_deref(), Some("EXAMPLE"));
        assert_eq!(hop2.responders[&core].reply_ttl, Some(62));
    }

    #[tokio::test]
    async fn a_file_that_is_no_log_is_refused_and_half_a_line_skipped() {
        let path = log_path("refused");
        std::fs::write(&path, "hello\n").unwrap();
        let error = Replay::open(&path).await.err().unwrap();
        assert!(error.to_string().contains("is not a log"), "{error}");

        std::fs::remove_file(&path).unwrap();
        let cli = [
            "mtr-ng",
            "-n",
            "--record",
            path.to_str().unwrap(),
            "192.0.2.1",
        ];
        let mut live = MtrSession::new(Args::parse_from(cli)).await.unwrap();
        live.sent(0, 3);
        live.recorder.as_ref().unwrap().flush();
        drop(live);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"t":1.5,"event":"sent","ho"#).unwrap();
        let s = replay(&path).await;
        std::fs::remove_file(&path).unwrap();
        assert_eq!(s.hops[0].sent, 1);
        assert_eq!(s.flows(), 4, "flows 0 to 3");
        assert_eq!(s.target, "192.0.2.1");
    }

    #[tokio::test]
    async fn a_log_is_never_overwritten() {
        let path = log_path("exists");
        std::fs::write(&path, "keep\n").unwrap();
        let cli = [
            "mtr-ng",
            "-n",
            "--record",
            path.to_str().unwrap(),
            "192.0.2.1",
        ];
        let error = MtrSession::new(Args::parse_from(cli)).await.err().unwrap();
        assert!(format!("{error:#}").contains("cannot create"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep\n");
        std::fs::remove_file(&path).unwrap();
    }
}
