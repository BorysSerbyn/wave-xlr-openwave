use crate::{
    meter::{CaptureReadiness, CaptureTap, PcmPeak, nonblocking},
    process::{CommandRunner, OwnedChild},
};
use openwave_core::{
    health::parse_health_graph,
    model::{NodeIdentity, OperationError, Result},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::Read,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const STARTUP: Duration = Duration::from_secs(1);
const WEDGE: Duration = Duration::from_secs(3);
const SILENCE: Duration = Duration::from_secs(30);
const MUTE_RECHECK: Duration = Duration::from_secs(5);
/// How long an armed Wave route is trusted to stay armed. WirePlumber's own
/// restore (after a wake, replug or restart) disarms it without any visible
/// change, so it is armed again on this interval.
pub const REARM: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinStatus {
    Absent,
    Starting,
    Unknown,
    Healthy,
    Wedged,
    Silent,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinAction {
    Keep,
    Restart,
}

/// Watchdog policy uses monotonic instants supplied by its owner. Its silence
/// allowance belongs to the physical node name, not a helper-process lifetime.
pub struct PinWatch {
    started: Instant,
    last_data: Instant,
    silence_since: Instant,
    received: bool,
    silence_spent: bool,
    status: PinStatus,
}
impl PinWatch {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            last_data: now,
            silence_since: now,
            received: false,
            silence_spent: false,
            status: PinStatus::Starting,
        }
    }
    pub fn restarted(&mut self, now: Instant) {
        self.started = now;
        self.last_data = now;
        self.silence_since = now;
        self.received = false;
        self.status = PinStatus::Starting;
        // Neither helper start nor a mute observation rearms silence recovery.
    }
    pub fn bytes(&mut self, at: Instant, nonzero: bool) {
        self.last_data = self.last_data.max(at);
        self.received = true;
        if nonzero {
            self.silence_since = at;
            self.silence_spent = false;
        }
    }
    pub fn status(&self) -> PinStatus {
        self.status
    }
    pub fn step(
        &mut self,
        now: Instant,
        alive: bool,
        graph_known: bool,
        muted: Option<bool>,
    ) -> PinAction {
        if !graph_known || muted.is_none() {
            self.silence_since = now;
            self.status = PinStatus::Unknown;
            return PinAction::Keep;
        }
        if !alive {
            self.status = PinStatus::Starting;
            return PinAction::Restart;
        }
        if now.saturating_duration_since(self.started) < STARTUP {
            self.status = PinStatus::Starting;
            return PinAction::Keep;
        }
        if now.saturating_duration_since(self.last_data) >= WEDGE {
            self.status = PinStatus::Wedged;
            return PinAction::Restart;
        }
        if !self.received {
            self.status = PinStatus::Starting;
            return PinAction::Keep;
        }
        if muted == Some(true) {
            self.silence_since = now;
            self.status = PinStatus::Healthy;
            return PinAction::Keep;
        }
        if now.saturating_duration_since(self.silence_since) >= SILENCE {
            self.status = PinStatus::Silent;
            if !self.silence_spent {
                self.silence_spent = true;
                return PinAction::Restart;
            }
        } else {
            self.status = PinStatus::Healthy;
        }
        PinAction::Keep
    }
}

pub fn aggregate_status(states: impl IntoIterator<Item = PinStatus>) -> PinStatus {
    states
        .into_iter()
        .max_by_key(|state| match state {
            PinStatus::Absent => 0,
            PinStatus::Healthy => 1,
            PinStatus::Starting => 2,
            PinStatus::Unknown => 3,
            PinStatus::Silent => 4,
            PinStatus::Wedged => 5,
        })
        .unwrap_or(PinStatus::Absent)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureNode {
    pub name: String,
    pub identity: NodeIdentity,
    pub muted: Option<bool>,
}
/// Unknown/ambiguous mute stays unknown; a malformed graph is not an unplug.
pub fn capture_nodes(value: &Value) -> Result<Vec<CaptureNode>> {
    let graph = parse_health_graph(value)?;
    let objects = value
        .as_array()
        .ok_or_else(|| OperationError::unavailable("invalid capture graph"))?;
    graph
        .captures
        .into_iter()
        .map(|(name, node)| {
            let object = objects
                .iter()
                .find(|v| integer(&v["id"]) == Some(node.node_id))
                .ok_or_else(|| {
                    OperationError::unavailable("capture node disappeared from graph")
                })?;
            // pw-cat numeric targets are serials, not node IDs. The core's
            // cookie-scoped node-ID identity fallback is not transfer authority.
            if object["info"]["props"].get("object.serial").is_none() {
                return Err(OperationError::unavailable(format!(
                    "Capture {name} has no targetable object.serial"
                )));
            }
            let device = integer(&object["info"]["props"]["device.id"]);
            let muted = device
                .and_then(|device| objects.iter().find(|v| integer(&v["id"]) == Some(device)))
                .and_then(|v| v["info"]["params"]["Route"].as_array())
                .and_then(|routes| {
                    let mut result = None;
                    for route in routes
                        .iter()
                        .filter(|route| route["direction"].as_str() == Some("Input"))
                    {
                        let mute = route["props"]["mute"].as_bool()?;
                        if result.is_some_and(|old| old != mute) {
                            return None;
                        }
                        result = Some(mute);
                    }
                    result
                });
            Ok(CaptureNode {
                name,
                identity: node.identity,
                muted,
            })
        })
        .collect()
}
/// An active route (mic or headphone path) of a Wave card in PipeWire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveRoute {
    /// PipeWire device object.
    pub object: u32,
    /// Route index and the card device it belongs to.
    pub index: u32,
    pub device: u32,
    pub name: String,
}
/// Active routes of the Wave cards in a pw-dump.
///
/// WirePlumber stores a route's volume only while the route carries a "save"
/// flag. A volume change made through a desktop mixer sets it; WirePlumber's
/// own restore clears it. A Wave is adjusted with its knobs, so its flag stays
/// clear, the volume the device reports is never stored, and the next restore
/// writes a stale value (100% for an unsaved mic, which is maximum gain).
pub fn wave_routes(value: &Value) -> Vec<WaveRoute> {
    let Some(objects) = value.as_array() else {
        return Vec::new();
    };
    let mut routes = Vec::new();
    for object in objects {
        let props = &object["info"]["props"];
        let is_wave = object["type"].as_str() == Some("PipeWire:Interface:Device")
            && props["device.api"].as_str() == Some("alsa")
            && props["device.name"].as_str().is_some_and(|name| {
                name.starts_with("alsa_card.usb-Elgato_Systems_Elgato_Wave")
                    || name.starts_with("alsa_card.usb-Elgato_Systems_Elgato_XLR_Dock")
            });
        let (true, Some(id)) = (is_wave, integer(&object["id"])) else {
            continue;
        };
        for route in object["info"]["params"]["Route"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let (Some(index), Some(device)) =
                (integer(&route["index"]), integer(&route["device"]))
            {
                routes.push(WaveRoute {
                    object: id,
                    index,
                    device,
                    name: route["name"].as_str().unwrap_or_default().to_owned(),
                });
            }
        }
    }
    routes
}
/// Keeps WirePlumber's save flag set on the Wave routes, so the volume and
/// gain the device reports (knob, touch mute, OpenWave) are what WirePlumber
/// stores and restores. Arming changes no volume and writes nothing to the
/// device; it only marks the route as one to remember.
#[derive(Debug, Default)]
pub struct RouteMemory {
    armed: HashMap<(u32, u32, u32), Instant>,
}
impl RouteMemory {
    /// Routes to arm now: new ones at once, the rest every [`REARM`].
    pub fn due(&mut self, routes: &[WaveRoute], now: Instant) -> Vec<WaveRoute> {
        self.armed.retain(|key, _| {
            routes
                .iter()
                .any(|route| (route.object, route.index, route.device) == *key)
        });
        routes
            .iter()
            .filter(|route| {
                let key = (route.object, route.index, route.device);
                match self.armed.get(&key) {
                    Some(at) if now.saturating_duration_since(*at) < REARM => false,
                    _ => {
                        self.armed.insert(key, now);
                        true
                    }
                }
            })
            .cloned()
            .collect()
    }
}
/// pw-cli arguments that set only the route's save flag.
pub fn arm_route_args(route: &WaveRoute) -> Vec<String> {
    vec![
        "set-param".into(),
        route.object.to_string(),
        "Route".into(),
        format!(
            "{{ \"index\": {}, \"device\": {}, \"save\": true }}",
            route.index, route.device
        ),
    ]
}
fn arm_routes(runner: &CommandRunner, memory: &mut RouteMemory, graph: &Value, now: Instant) {
    for route in memory.due(&wave_routes(graph), now) {
        if let Err(error) = runner.run("pw-cli", &arm_route_args(&route), Duration::from_secs(3)) {
            log::warn!(
                "Could not ask WirePlumber to remember {} of device {}: {error}",
                route.name,
                route.object
            );
        }
    }
}
fn integer(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|v| v.try_into().ok())
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
}

#[derive(Default)]
struct Samples {
    last_data: Option<Instant>,
    last_nonzero: Option<Instant>,
    exited: bool,
}
struct Reader {
    cancel: Arc<AtomicBool>,
    samples: Arc<Mutex<Samples>>,
    tap: Arc<CaptureTap>,
    join: JoinHandle<Result<()>>,
}
struct Pin {
    node: CaptureNode,
    watch: PinWatch,
    reader: Option<Reader>,
    last_sample: Option<Instant>,
    last_signal: Option<Instant>,
    last_flow: Option<Instant>,
    mute_checked: Instant,
}
impl Pin {
    fn start(&mut self, bank: &CaptureReadiness) -> Result<()> {
        self.stop()?;
        let now = Instant::now();
        self.watch.restarted(now);
        self.last_sample = None;
        self.last_signal = None;
        let tap = Arc::new(bank.register(self.node.identity.clone(), now, STARTUP)?);
        let previous_flow = *self.last_flow.get_or_insert(now);
        let samples = Arc::new(Mutex::new(Samples::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let (name, reader_tap, reader_samples, reader_cancel) = (
            self.node.name.clone(),
            tap.clone(),
            samples.clone(),
            cancel.clone(),
        );
        let serial = self.node.identity.object_serial.clone();
        let join = thread::Builder::new()
            .name("openwave-capture-pin".into())
            .spawn(move || {
                let result = drain_pin(
                    &name,
                    &serial,
                    &reader_cancel,
                    &reader_samples,
                    &reader_tap,
                    previous_flow,
                );
                reader_samples
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exited = true;
                reader_tap.invalidate();
                result
            })?;
        self.reader = Some(Reader {
            cancel,
            samples,
            tap,
            join,
        });
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        if let Some(reader) = self.reader.take() {
            reader.cancel.store(true, Ordering::Release);
            reader.tap.invalidate();
            reader
                .join
                .join()
                .map_err(|_| OperationError::unavailable("capture reader panicked"))??;
        }
        Ok(())
    }
    fn observe(&mut self) -> bool {
        let Some(reader) = &self.reader else {
            return false;
        };
        let samples = match reader.samples.lock() {
            Ok(samples) => samples,
            Err(_) => {
                self.node.muted = None;
                return false;
            }
        };
        if let Some(at) = samples.last_data.filter(|at| Some(*at) != self.last_sample) {
            self.watch.bytes(at, false);
            self.last_sample = Some(at);
            self.last_flow = Some(at);
        }
        if let Some(at) = samples
            .last_nonzero
            .filter(|at| Some(*at) != self.last_signal)
        {
            self.watch.bytes(at, true);
            self.last_signal = Some(at);
        }
        !samples.exited
    }
}
impl Drop for Pin {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn drain_pin(
    name: &str,
    serial: &str,
    cancel: &AtomicBool,
    samples: &Mutex<Samples>,
    tap: &CaptureTap,
    previous_flow: Instant,
) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let properties = serde_json::json!({"node.name":format!("openwave_keepalive_{name}"), "node.description":"OpenWave capture keepalive", "media.name":format!("OpenWave keepalive: {name}"), "application.name":"OpenWave", "node.dont-fallback":true, "node.dont-move":true, "node.dont-reconnect":true});
    let args = vec![
        "--record".into(),
        "--target".into(),
        serial.into(),
        "--channels".into(),
        "1".into(),
        "--format".into(),
        "s16".into(),
        "--rate".into(),
        "48000".into(),
        "--latency".into(),
        "200ms".into(),
        "--properties".into(),
        properties.to_string(),
        "-".into(),
    ];
    let mut child = OwnedChild::spawn("pw-cat", &args, Stdio::piped())?;
    tap.started(Instant::now(), Some(previous_flow))?;
    log::info!("Started capture keepalive for {name} (PID {})", child.id());
    let result = (|| -> Result<()> {
        let mut stdout = child
            .take_stdout()
            .ok_or_else(|| OperationError::unavailable("capture stdout unavailable"))?;
        nonblocking(&stdout)?;
        let mut buffer = [0u8; 4096];
        let mut pcm = PcmPeak::default();
        while !cancel.load(Ordering::Acquire) {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    let now = Instant::now();
                    tap.received(now)?;
                    let peak = pcm.push(&buffer[..size]);
                    let mut state = samples
                        .lock()
                        .map_err(|_| OperationError::unavailable("capture sample bank poisoned"))?;
                    state.last_data = Some(now);
                    if peak.is_some_and(|peak| peak > 0.0) {
                        state.last_nonzero = Some(now);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10))
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    let termination = child.terminate();
    log::info!("Stopped capture keepalive for {name}");
    result.and(termination)
}

pub struct AudioManager {
    cancel: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    readiness: CaptureReadiness,
    join: Option<JoinHandle<Result<()>>>,
}
impl AudioManager {
    pub fn start() -> Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let readiness = CaptureReadiness::default();
        let (wake, receiver) = mpsc::channel();
        let (worker_cancel, worker_bank) = (cancel.clone(), readiness.clone());
        let join = thread::Builder::new()
            .name("openwave-audio".into())
            .spawn(move || audio_loop(worker_cancel, worker_bank, receiver))?;
        Ok(Self {
            cancel,
            wake,
            readiness,
            join: Some(join),
        })
    }
    pub fn readiness(&self) -> CaptureReadiness {
        self.readiness.clone()
    }
    pub fn stop(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::Release);
        let _ = self.wake.send(());
        match self.join.take() {
            Some(join) => join
                .join()
                .map_err(|_| OperationError::unavailable("audio manager panicked"))?,
            None => Ok(()),
        }
    }
}
impl Drop for AudioManager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn audio_loop(
    cancel: Arc<AtomicBool>,
    bank: CaptureReadiness,
    wake: mpsc::Receiver<()>,
) -> Result<()> {
    let runner = CommandRunner::new(cancel.clone());
    let mut pins: HashMap<String, Pin> = HashMap::new();
    let mut dormant: HashMap<String, PinWatch> = HashMap::new();
    let mut previous = None;
    let mut absent_retry = Duration::from_millis(100);
    let mut shutdown_errors = Vec::new();
    let mut memory = RouteMemory::default();
    while !cancel.load(Ordering::Acquire) {
        let graph = runner
            .run("pw-dump", &["--no-colors".into()], Duration::from_secs(3))
            .and_then(|output| Ok(serde_json::from_slice::<Value>(&output.stdout)?));
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let now = Instant::now();
        if let Ok(graph) = &graph {
            arm_routes(&runner, &mut memory, graph, now);
        }
        let observation = graph.and_then(|value| capture_nodes(&value));
        let nodes = match observation {
            Ok(nodes) => nodes,
            Err(error) => {
                log::warn!("Capture graph unknown: {error}");
                for pin in pins.values_mut() {
                    let alive = pin.observe();
                    pin.watch.step(now, alive, false, None);
                }
                report_status(PinStatus::Unknown, &mut previous);
                let _ = wake.recv_timeout(STARTUP);
                continue;
            }
        };
        let retired: Vec<_> = pins
            .iter()
            .filter(|(name, pin)| {
                !nodes
                    .iter()
                    .any(|node| &node.name == *name && node.identity == pin.node.identity)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in retired {
            let mut pin = pins.remove(&name).unwrap();
            pin.observe();
            if let Err(error) = pin.stop() {
                log::error!("Stopping capture {name}: {error}");
                shutdown_errors.push(error.to_string());
            }
            let watch = std::mem::replace(&mut pin.watch, PinWatch::new(now));
            dormant.insert(name, watch);
        }
        for node in nodes {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            if let Some(pin) = pins.get_mut(&node.name) {
                if now.saturating_duration_since(pin.mute_checked) >= MUTE_RECHECK {
                    pin.node.muted = node.muted;
                    pin.mute_checked = now;
                }
            } else {
                let watch = dormant
                    .remove(&node.name)
                    .unwrap_or_else(|| PinWatch::new(now));
                let mut pin = Pin {
                    node,
                    watch,
                    reader: None,
                    last_sample: None,
                    last_signal: None,
                    last_flow: None,
                    mute_checked: now,
                };
                if let Err(error) = pin.start(&bank) {
                    log::error!("Starting capture {}: {error}", pin.node.name);
                }
                pins.insert(pin.node.name.clone(), pin);
            }
        }
        for pin in pins.values_mut() {
            let alive = pin.observe();
            if pin.watch.step(now, alive, true, pin.node.muted) == PinAction::Restart {
                log::warn!(
                    "Capture {} {:?}; recycling owned keepalive",
                    pin.node.name,
                    pin.watch.status()
                );
                if let Err(error) = pin.start(&bank) {
                    log::error!("Restarting capture {}: {error}", pin.node.name);
                }
            }
        }
        report_status(
            aggregate_status(pins.values().map(|pin| pin.watch.status())),
            &mut previous,
        );
        let delay = if pins.is_empty() {
            let delay = absent_retry;
            absent_retry = (absent_retry * 2).min(MUTE_RECHECK);
            delay
        } else {
            absent_retry = Duration::from_millis(100);
            STARTUP
        };
        let _ = wake.recv_timeout(delay);
    }
    for (_, mut pin) in pins {
        if let Err(error) = pin.stop() {
            shutdown_errors.push(error.to_string());
        }
    }
    if shutdown_errors.is_empty() {
        Ok(())
    } else {
        Err(OperationError::unavailable(shutdown_errors.join("; ")))
    }
}
fn report_status(status: PinStatus, previous: &mut Option<PinStatus>) {
    if *previous == Some(status) {
        return;
    }
    *previous = Some(status);
    match status {
        PinStatus::Absent => log::info!("Device not detected"),
        PinStatus::Healthy => log::info!("Capture keepalive active"),
        PinStatus::Silent => {
            log::error!("Capture delivers digital silence; power-cycle the device")
        }
        PinStatus::Unknown => {
            log::warn!("Capture state unavailable; preserving pins without recovery")
        }
        PinStatus::Starting | PinStatus::Wedged => log::warn!("Establishing capture keepalive..."),
    }
}
