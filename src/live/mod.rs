mod advisor_experiment;
pub(crate) mod compatibility;
mod diagnostics;
mod engine;
mod native_current;
mod observation;
mod powers;
mod relics;
mod session;
mod sim_observation;
#[cfg(test)]
mod tests;
mod transport;

use std::{
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sts2_e2e::net::E2eNet;

use crate::{Failure, Result, model};

use observation::Observation;
use session::{Ack, Event, Position, Session};
use transport::{Frames, MAX_FRAME, Receive};

const READ_POLL: Duration = Duration::from_millis(50);
const RECONNECT: Duration = Duration::from_millis(500);

struct Job {
    position: Position,
    received: Instant,
    observation: Observation,
}

#[derive(Default)]
struct Latest {
    position: Option<Position>,
    received: Option<Instant>,
    display: bool,
    confirmed: bool,
    pending: Option<Job>,
    completed: Option<(Instant, Value)>,
}

impl Latest {
    fn new_position(&mut self, p: Position, t: Instant) {
        *self = Self {
            position: Some(p),
            received: Some(t),
            ..Self::default()
        };
    }
    fn valid(&self, p: Position, now: Instant, ttl: Duration) -> bool {
        self.position == Some(p)
            && self
                .received
                .is_some_and(|t| now.saturating_duration_since(t) < ttl)
    }

    fn confirm(&mut self, p: Position, at: Instant, ttl: Duration) -> bool {
        if !self.valid(p, at, ttl) {
            return false;
        }
        self.received = Some(at);
        self.confirmed = true;
        true
    }
}

fn observation_id(obs: &Observation) -> String {
    let mut board = obs.board.clone();
    let fields = board.as_object_mut().expect("sanitized observation object");
    fields.remove("state_id");
    fields.remove("request_id");
    format!("{:x}", Sha256::digest(board.to_string().as_bytes()))
}

struct Terminal {
    json: bool,
}

impl Terminal {
    fn emit(&self, event: &Value) {
        if event["status"] == "confirmed" && !self.json {
            return;
        }
        let mut out = io::stdout().lock();
        if self.json {
            static STARTED: OnceLock<Instant> = OnceLock::new();
            let mut event = event.clone();
            event["elapsed_ms"] = json!(STARTED.get_or_init(Instant::now).elapsed().as_millis());
            let _ = writeln!(out, "{event}");
        } else {
            let status = event["status"].as_str().unwrap_or("unknown");
            let _ = writeln!(out, "\x1b[2J\x1b[HSpireMind | {status}");
            if let Some(p) = event.get("position") {
                let _ = writeln!(
                    out,
                    "State {} / session {} | {} | {} | Ironclad A{}",
                    p["state_id"],
                    p["session"],
                    event["context"].as_str().unwrap_or(""),
                    event["game_version"].as_str().unwrap_or(""),
                    event["ascension"]
                );
            }
            if let Some(reason) = event["error"]["message"]
                .as_str()
                .or_else(|| event["reason"].as_str())
            {
                let _ = writeln!(out, "{reason}");
            }
            let actions = event["legal_actions"].as_array();
            let row =
                |id: &Value| actions.and_then(|a| id.as_u64().and_then(|i| a.get(i as usize)));
            if let Some(id) = event.get("forced_action_id")
                && let Some(a) = row(id)
            {
                let _ = writeln!(out, "Only legal action: {}", label(&a["action"]));
            }
            if let Some(ranked) = event["recommendations"].as_array() {
                for (i, recommendation) in ranked.iter().take(5).enumerate() {
                    if let Some(a) = row(&recommendation["representative_action_id"]) {
                        let score =
                            recommendation["policy_probability"].as_f64().unwrap_or(0.0) * 100.0;
                        let _ = writeln!(
                            out,
                            "{}. {}  ({score:.1}% policy)",
                            i + 1,
                            label(&a["action"])
                        );
                    }
                }
            }
            if let Some(ms) = event["receive_to_result_ms"].as_f64() {
                let forward = event["forward_ms"]
                    .as_f64()
                    .map_or_else(|| "—".to_owned(), |v| format!("{v:.2} ms"));
                let _ = writeln!(out, "\nReceive to result {ms:.2} ms | forward {forward}");
            }
        }
        let _ = out.flush();
    }
}

fn label(a: &Value) -> String {
    let visible = match a["kind"].as_str() {
        Some("play") => {
            let mut text = format!(
                "{} (hand {}, cost {})",
                a["card"]["name"].as_str().unwrap_or("?"),
                a["card"]["hand_index"],
                a["card"]["effective_cost"]
            );
            if let Some(name) = a["target"]["name"].as_str() {
                text.push_str(&format!(" → {name} (enemy {})", a["target"]["enemy_index"]));
            }
            text
        }
        Some("reward_card") => format!(
            "{} (offer {})",
            a["name"].as_str().unwrap_or("?"),
            a["offer_index"]
        ),
        Some("potion") => format!(
            "{} (potion {})",
            a["name"].as_str().unwrap_or("?"),
            a["slot"]
        ),
        Some("move") => format!(
            "{} (row {}, col {})",
            a["room_type"].as_str().unwrap_or("?"),
            a["row"],
            a["col"]
        ),
        Some("end_turn") => "End turn".to_owned(),
        Some("skip") => "Skip".to_owned(),
        _ => "Unknown action".to_owned(),
    };
    visible.chars().filter(|c| !c.is_control()).collect()
}

fn decorate(mut event: Value, obs: &Observation, p: Position, source: &str) -> Value {
    event["schema_version"] = json!(1);
    event["source"] = json!(source);
    event["position"] = json!(p);
    event["context"] = json!(obs.context);
    event["game_version"] = obs.board["game_version"].clone();
    event["mod_version"] = obs.board["mod_version"].clone();
    event["character"] = obs.board["run_state"]["character"].clone();
    event["ascension"] = obs.board["run_state"]["ascension"].clone();
    event["model"] = json!(model::pin());
    event["upstream_revision"] = json!(model::REVISION);
    event["observation"] = json!(if obs.board.get("native_current").is_some() {
        native_current::PROFILE
    } else if obs.context == crate::contract::Context::Combat {
        model::OBSERVED_COMBAT
    } else {
        model::OBSERVATION
    });
    event
}

fn blocked(e: Failure) -> Value {
    json!({"schema_version":1,"status":"blocked","error":e})
}

fn persist(dir: &Path, job: &Job) -> io::Result<()> {
    let path = dir.join(format!(
        "advisor-{}-{}-{}.json",
        std::process::id(),
        job.position.session,
        job.position.state_id
    ));
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer(&mut file, &job.observation.board)?;
    file.write_all(b"\n")
}

struct Live {
    native_current: bool,
    latest: Mutex<Latest>,
    cache: Mutex<Option<CachedAdvice>>,
    blocked_corpus: Mutex<Option<diagnostics::Corpus>>,
    wake: Condvar,
    terminal: Terminal,
    ttl: Duration,
}

struct CachedAdvice {
    session: u64,
    observation_id: String,
    result: Value,
}

impl Live {
    fn worker(&self, net: E2eNet, capture: Option<PathBuf>) {
        loop {
            let job = {
                let state = self.latest.lock().unwrap();
                let mut guard = self
                    .wake
                    .wait_while(state, |s| s.pending.is_none())
                    .unwrap();
                guard.pending.take().unwrap()
            };
            let id = observation_id(&job.observation);
            let cached = self
                .cache
                .lock()
                .unwrap()
                .as_ref()
                .filter(|entry| entry.session == job.position.session && entry.observation_id == id)
                .map(|entry| entry.result.clone());
            let (value, reused) = match cached {
                Some(advice) => (advice, true),
                None => (
                    engine::advise(&net, &job.observation).unwrap_or_else(blocked),
                    false,
                ),
            };
            let mut result = decorate(value, &job.observation, job.position, "advisor_tcp");
            if let Some(code) = result["error"]["code"].as_str() {
                self.record_block(
                    &job.observation.board,
                    job.position,
                    "engine",
                    code,
                    Some(&id),
                );
            }
            result["observation_id"] = json!(id);
            result["reused"] = json!(reused);
            {
                let mut guard = self.latest.lock().unwrap();
                let now = Instant::now();
                if guard.valid(job.position, now, self.ttl) {
                    guard.completed = Some((job.received, result));
                    self.publish(&mut guard, now);
                }
            }
            if let Some(dir) = capture.as_deref()
                && persist(dir, &job).is_err()
            {
                eprintln!("SpireMind: could not save sanitized observation");
            }
        }
    }

    fn record_block(
        &self,
        raw: &Value,
        p: Position,
        phase: &'static str,
        code: &str,
        id: Option<&str>,
    ) {
        if let Some(corpus) = self.blocked_corpus.lock().unwrap().as_mut()
            && corpus
                .write(|| diagnostics::record(raw, p, phase, code, id))
                .is_err()
        {
            eprintln!("SpireMind: could not save redacted blocker diagnostics");
        }
    }

    fn publish(&self, latest: &mut Latest, now: Instant) {
        if latest.confirmed
            && let Some((received, mut result)) = latest.completed.take()
        {
            if result["reused"] == false
                && matches!(result["status"].as_str(), Some("ok" | "forced"))
            {
                let mut reusable = result.clone();
                reusable.as_object_mut().unwrap().remove("forward_ms");
                *self.cache.lock().unwrap() = Some(CachedAdvice {
                    session: latest.position.unwrap().session,
                    observation_id: result["observation_id"].as_str().unwrap().to_owned(),
                    result: reusable,
                });
            }
            result["receive_to_result_ms"] =
                json!(now.duration_since(received).as_secs_f64() * 1000.0);
            self.terminal.emit(&result);
            latest.display = true;
        }
    }

    fn clear(&self, latest: &mut Latest, reason: &'static str) {
        if let Some(position) = latest.position {
            *latest = Latest::default();
            self.terminal.emit(
                &json!({"schema_version":1,"status":"stale","reason":reason,"position":position}),
            );
        }
    }

    fn expire(&self) -> bool {
        let mut l = self.latest.lock().unwrap();
        if l.position.is_some() && l.received.is_some_and(|t| t.elapsed() >= self.ttl) {
            self.clear(&mut l, "observation_ttl");
            return true;
        }
        false
    }

    fn begin(&self, p: Position, t: Instant) {
        let mut l = self.latest.lock().unwrap();
        let previous = l.position.filter(|_| l.display);
        l.new_position(p, t);
        if previous.is_some() {
            self.terminal.emit(&json!({"schema_version":1,"status":"stale","reason":"new_observation","position":previous}));
        }
    }

    fn queue(&self, job: Job) {
        let mut l = self.latest.lock().unwrap();
        if l.valid(job.position, Instant::now(), self.ttl) {
            l.pending = Some(job);
            self.wake.notify_one();
        }
    }

    fn connection(&self, mut socket: TcpStream, id: u64, expected_build: &str) -> Result<()> {
        socket
            .set_read_timeout(Some(READ_POLL))
            .map_err(|_| link_error())?;
        socket
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(|_| link_error())?;
        socket.set_nodelay(true).map_err(|_| link_error())?;
        self.serve(&mut socket, id, expected_build)
    }

    fn serve(&self, socket: &mut (impl Read + Write), id: u64, expected_build: &str) -> Result<()> {
        self.terminal.emit(&json!({"schema_version":1,"status":"waiting","reason":"Connected. Waiting for Advisor observations."}));
        let mut frames = Frames::default();
        let mut session = Session::new(id);
        loop {
            if self.expire() {
                // A gap invalidates the lease. A fresh connection obtains a
                // fresh request instead of reviving the position from a late pulse.
                return Ok(());
            }
            match frames.read(socket).map_err(|_| link_error())? {
                Receive::Idle => continue,
                Receive::Eof => return Ok(()),
                Receive::Frame(bytes, at) => {
                    let raw: Value = serde_json::from_slice(&bytes)
                        .map_err(|_| observation::schema("JSONL frame"))?;
                    drop(bytes);
                    match session.accept(&raw)? {
                        Event::Ignore => {}
                        Event::Invalidate(reason) => {
                            self.clear(&mut self.latest.lock().unwrap(), reason)
                        }
                        Event::Withdraw(p) => {
                            let mut current = self.latest.lock().unwrap();
                            if current.position == Some(p) {
                                self.clear(&mut current, "producer_withdraw");
                            }
                        }
                        Event::Confirm(p, seq) => {
                            let mut current = self.latest.lock().unwrap();
                            if current.confirm(p, at, self.ttl) {
                                if self.terminal.json {
                                    self.terminal.emit(&json!({"schema_version":1,"status":"confirmed","position":p,"lease_seq":seq}));
                                }
                                self.publish(&mut current, Instant::now());
                            }
                        }
                        Event::Observation(p) => {
                            let actual = observation::string(&raw, "game_version")?;
                            observation::live_build(actual)?;
                            if expected_build != actual
                                || raw["mod_version"] != observation::MOD
                                || raw["player_count"] != 1
                            {
                                return Err(Failure::new(
                                    "producer_mismatch",
                                    "Version, mod or player count differs from the admitted producer",
                                ));
                            }
                            if raw["capabilities"]["spiremind_lease"] != 1 {
                                return Err(Failure::new(
                                    "lease_not_supported",
                                    "Install the pinned SpireMind observer patch before using live",
                                ));
                            }
                            observation::confirm_power_capability(&raw)?;
                            let has_native = raw["capabilities"][native_current::CAPABILITY]
                                == native_current::VERSION;
                            if self.native_current != has_native {
                                return Err(Failure::new(
                                    "producer_profile",
                                    "The staged native-current mod and --native-current must be used together",
                                ));
                            }
                            self.begin(p, at);
                            // A screen request remains in flight without this reply.
                            // The Ack has no best, options, gameplay fields or score.
                            let mut ack = serde_json::to_vec(&Ack::for_position(p))
                                .expect("serializable fixed ack");
                            ack.push(b'\n');
                            socket.write_all(&ack).map_err(|_| link_error())?;
                            let obs = if self.native_current {
                                session.observe_native(&raw, p)
                            } else {
                                session.observe(&raw, p)
                            };
                            match obs {
                                Ok(observation) => self.queue(Job {
                                    position: p,
                                    received: at,
                                    observation,
                                }),
                                Err(e) => {
                                    let context = observation::context(&raw).ok();
                                    let native_combat = self.native_current
                                        && context == Some(crate::contract::Context::Combat);
                                    if !native_combat {
                                        self.record_block(&raw, p, "observation", e.code, None);
                                    }
                                    let mut event = blocked(e);
                                    event["position"] = json!(p);
                                    event["context"] = json!(context);
                                    if native_combat {
                                        event["source"] = json!("advisor_tcp");
                                        event["observation"] = json!(native_current::PROFILE);
                                        if let Ok(board) = native_current::project_frame(&raw) {
                                            event["observation_id"] =
                                                json!(observation_id(&Observation {
                                                    board,
                                                    context: crate::contract::Context::Combat,
                                                }));
                                        }
                                    }
                                    self.terminal.emit(&event);
                                    // Unsupported/ill-formed screens may have no lease.
                                    *self.latest.lock().unwrap() = Latest::default();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn link_error() -> Failure {
    Failure::new(
        "transport",
        "Advisor connection ended or exceeded a frame/IO limit",
    )
}

pub struct LiveProducer<'a> {
    pub build: &'a str,
    pub native_current: bool,
}

pub fn run(
    net: E2eNet,
    port: u16,
    ttl_ms: u64,
    json_output: bool,
    capture: Option<PathBuf>,
    blocked_corpus: Option<PathBuf>,
    producer: LiveProducer<'_>,
) -> Result<()> {
    if let Some(path) = capture.as_deref() {
        fs::create_dir_all(path)
            .map_err(|_| Failure::new("capture_io", "Cannot create capture directory"))?;
    }
    let live = Arc::new(Live {
        native_current: producer.native_current,
        latest: Mutex::new(Latest::default()),
        cache: Mutex::new(None),
        blocked_corpus: Mutex::new(
            blocked_corpus
                .map(diagnostics::Corpus::new)
                .transpose()
                .map_err(|_| {
                    Failure::new(
                        "capture_io",
                        "Blocked corpus must be a new writable directory",
                    )
                })?,
        ),
        wake: Condvar::new(),
        terminal: Terminal {
            json: json_output || !io::stdout().is_terminal(),
        },
        ttl: Duration::from_millis(ttl_ms),
    });
    let worker = Arc::clone(&live);
    thread::Builder::new()
        .name("instinct-live".into())
        .spawn(move || worker.worker(net, capture))
        .map_err(|_| Failure::new("worker", "Cannot start inference worker"))?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    live.terminal.emit(&json!({"schema_version":1,"status":"waiting","reason":format!("Waiting for Spire Mentor on {address}")}));
    let mut session_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    loop {
        if let Ok(socket) = TcpStream::connect_timeout(&address, RECONNECT) {
            session_id += 1;
            let reason = live.connection(socket, session_id, producer.build).err();
            live.clear(&mut live.latest.lock().unwrap(), "disconnect");
            if let Some(error) = reason {
                if matches!(
                    error.code,
                    "lease_not_supported"
                        | "producer_mismatch"
                        | "incompatible_game_version"
                        | "producer_profile"
                ) {
                    return Err(error);
                }
                live.terminal.emit(&blocked(error));
            }
        }
        thread::sleep(RECONNECT);
    }
}

fn read_capture(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(MAX_FRAME as u64 + 1).read_to_end(&mut bytes))
        .map_err(|_| Failure::new("capture_io", "Cannot read capture file"))?;
    if bytes.len() > MAX_FRAME {
        return Err(observation::schema("bounded capture size"));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| observation::schema("capture JSON"))?;
    if value["kind"] == "blocked_diagnostic" {
        return Err(Failure::new(
            "diagnostic_not_replayable",
            "A blocked diagnostic is not a validated observation",
        ));
    }
    Ok(value)
}

pub use observation::live_build as check_version;

pub fn release_build(path: &Path) -> Result<String> {
    let value = read_capture(path)?;
    let version = observation::string(&value, "version")?;
    check_version(version)?;
    let commit = observation::string(&value, "commit")?;
    if !matches!(
        (version, commit),
        ("v0.111.0", "41cef1ea") | ("v0.111.1", "cc1f9462")
    ) {
        return Err(Failure::new(
            "incompatible_game_version",
            "Unverified release_info.json version and commit pair",
        ));
    }
    Ok(version.to_owned())
}

pub fn sanitize_file(input: &Path, output: &Path) -> Result<Value> {
    let raw = read_capture(input)?;
    let o = sanitize_capture(&raw)?;
    if o.board["map"].is_null() {
        return Err(Failure::new(
            "missing_visible_map",
            "Capture must contain a visible map",
        ));
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output)
        .map_err(|_| Failure::new("capture_io", "Cannot create new sanitized capture file"))?;
    serde_json::to_writer(&mut file, &o.board)
        .map_err(|_| Failure::new("capture_io", "Cannot save sanitized capture"))?;
    Ok(
        json!({"schema_version":1,"status":"sanitized","context":o.context,"state_id":o.board["state_id"]}),
    )
}

fn sanitize_capture(raw: &Value) -> Result<Observation> {
    if raw.get("native_current").is_some() {
        native_current::sanitize(raw)
    } else {
        observation::sanitize(raw, observation::context(raw)?)
    }
}

pub fn replay(net: &E2eNet, files: &[PathBuf]) -> Result<(Value, bool)> {
    let mut ok = true;
    let mut times = Vec::new();
    let mut results = Vec::new();
    for (index, path) in files.iter().enumerate() {
        let raw = read_capture(path)?;
        let at = Instant::now();
        let obs = sanitize_capture(&raw)?;
        let p = Position {
            session: 0,
            state_id: observation::number(&obs.board, "state_id", i64::MAX as u64)?,
            request_id: 0,
        };
        let mut value = decorate(
            engine::advise(net, &obs).unwrap_or_else(|e| {
                ok = false;
                blocked(e)
            }),
            &obs,
            p,
            "replay",
        );
        let ms = at.elapsed().as_secs_f64() * 1000.0;
        value["observation_id"] = json!(observation_id(&obs));
        if value["status"] == "ok" {
            times.push(ms);
        }
        value["replay_to_result_ms"] = json!(ms);
        value["replay_index"] = json!(index);
        results.push(value);
    }
    times.sort_by(f64::total_cmp);
    let percent = |p: usize| {
        if times.is_empty() {
            None
        } else {
            Some(times[(p * times.len()).div_ceil(100) - 1])
        }
    };
    Ok((
        json!({"schema_version":1,"source":"replay","status":if ok{"ok"}else{"partial"},"scored_latency":{"count":times.len(),"p50_ms":percent(50),"p95_ms":percent(95)},"observations":results}),
        ok,
    ))
}
