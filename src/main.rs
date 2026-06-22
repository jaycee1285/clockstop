use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(debug_assertions)]
use tracing::debug;
use tracing::{error, info, warn};
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Population = BTreeMap<String, u32>;

#[derive(Debug, Clone)]
struct Config {
    default_minutes: u64,
    threshold: Duration,
    tick: Duration,
    sample_cmd: String,
    lock_cmd: String,
    picker_cmd: String,
    log_path: String,
}

impl Config {
    fn from_env() -> Self {
        let default_minutes = read_u64("CLOCKSTOP_DEFAULT_MINUTES").unwrap_or(25);
        let threshold_seconds = read_u64("CLOCKSTOP_T_SECONDS").unwrap_or(20);
        let tick_seconds = read_u64("CLOCKSTOP_TICK_SECONDS").unwrap_or(2).max(1);
        Self {
            default_minutes,
            threshold: Duration::from_secs(threshold_seconds),
            tick: Duration::from_secs(tick_seconds),
            sample_cmd: std::env::var("CLOCKSTOP_SAMPLE_CMD")
                .unwrap_or_else(|_| "wlrctl toplevel list".to_string()),
            lock_cmd: std::env::var("CLOCKSTOP_LOCK_CMD").unwrap_or_else(|_| {
                "swaylock-effects --screenshots --effect-pixelate 10".to_string()
            }),
            picker_cmd: std::env::var("CLOCKSTOP_PICKER_CMD")
                .unwrap_or_else(|_| "fuzzel --dmenu --prompt 'minutes> ' --minimal-lines --lines=6 --width=10".to_string()),
            log_path: std::env::var("CLOCKSTOP_LOG")
                .unwrap_or_else(|_| "clockstop-smoke.log".to_string()),
        }
    }
}

fn read_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

#[derive(Debug, Clone)]
struct SharedState {
    phase: Phase,
    default_minutes: u64,
    session: Option<SessionView>,
    last_event: String,
}

impl SharedState {
    fn idle(default_minutes: u64) -> Self {
        Self {
            phase: Phase::Idle,
            default_minutes,
            session: None,
            last_event: "ready".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Phase {
    Idle,
    Running,
    Drift,
    Locked,
    Cooldown,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Idle => "Idle",
            Phase::Running => "Running",
            Phase::Drift => "Drifting",
            Phase::Locked => "Locked",
            Phase::Cooldown => "Cooldown",
        }
    }
}

#[derive(Debug, Clone)]
struct SessionView {
    duration: Duration,
    remaining: Duration,
    bucket: Duration,
    snapshot: Population,
    delta: Population,
}

#[derive(Debug)]
enum AppCommand {
    Start(Duration),
    Stop,
    Status,
    Quit,
}

#[derive(Clone)]
struct TrayHandle {
    tx: Sender<TraySignal>,
}

impl TrayHandle {
    fn update(&self) {
        if let Err(err) = self.tx.send(TraySignal::Update) {
            error!("failed to request tray update: {err}");
        }
    }

    fn shutdown(&self) {
        if let Err(err) = self.tx.send(TraySignal::Shutdown) {
            error!("failed to request tray shutdown: {err}");
        }
    }
}

enum TraySignal {
    Update,
    Shutdown,
}

#[derive(Clone)]
struct ClockstopItem {
    state: Arc<Mutex<SharedState>>,
    config: Arc<Config>,
    tx: Sender<AppCommand>,
    picker_open: Arc<AtomicBool>,
}

impl ClockstopItem {
    fn send(&self, command: AppCommand) {
        if let Err(err) = self.tx.send(command) {
            error!("failed to send tray command: {err}");
        }
    }

    fn title_text(&self) -> String {
        let state = self.state.lock().unwrap();
        match &state.session {
            Some(session) => format!(
                "clockstop: {} {}m left",
                state.phase.label(),
                session.remaining.as_secs() / 60
            ),
            None => "clockstop: idle".to_string(),
        }
    }

    fn status_text(&self) -> String {
        match self.state.lock().unwrap().phase {
            Phase::Idle | Phase::Running | Phase::Cooldown => "Active".to_string(),
            Phase::Drift | Phase::Locked => "NeedsAttention".to_string(),
        }
    }

    fn icon_pixmap_data(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let color = match self.state.lock().unwrap().phase {
            Phase::Idle => (0x8a, 0x8f, 0x98),
            Phase::Running => (0x3f, 0xc9, 0x77),
            Phase::Drift => (0xf0, 0xb4, 0x29),
            Phase::Locked => (0xef, 0x44, 0x44),
            Phase::Cooldown => (0x60, 0xa5, 0xfa),
        };
        let icon = solid_clock_icon(32, color);
        vec![(icon.width, icon.height, icon.data)]
    }
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl ClockstopItem {
    #[zbus(signal)]
    async fn new_attention_icon(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_icon(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_overlay_icon(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_status(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
        status: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_tool_tip(
        signal_emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;

    fn activate(&self, _x: i32, _y: i32) {
        self.send(AppCommand::Start(Duration::from_secs(
            self.state.lock().unwrap().default_minutes * 60,
        )));
    }

    fn context_menu(&self, _x: i32, _y: i32) {
        open_custom_picker(
            self.config.clone(),
            self.tx.clone(),
            self.state.clone(),
            self.picker_open.clone(),
        );
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        open_custom_picker(
            self.config.clone(),
            self.tx.clone(),
            self.state.clone(),
            self.picker_open.clone(),
        );
    }

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    #[zbus(property)]
    fn id(&self) -> String {
        "clockstop".to_string()
    }

    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".to_string()
    }

    #[zbus(property)]
    fn title(&self) -> String {
        self.title_text()
    }

    #[zbus(property)]
    fn status(&self) -> String {
        self.status_text()
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        self.icon_pixmap_data()
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_movie_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        let title = self.title_text();
        let description = self.state.lock().unwrap().last_event.clone();
        (String::new(), Vec::new(), title, description)
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn menu(&self) -> zbus::fdo::Result<OwnedObjectPath> {
        OwnedObjectPath::try_from("/MenuBar").map_err(|err| {
            zbus::fdo::Error::Failed(format!("invalid clockstop menu path: {err}"))
        })
    }
}

#[derive(Clone)]
struct ClockstopMenu {
    state: Arc<Mutex<SharedState>>,
    config: Arc<Config>,
    tx: Sender<AppCommand>,
    picker_open: Arc<AtomicBool>,
}

impl ClockstopMenu {
    fn send(&self, command: AppCommand) {
        if let Err(err) = self.tx.send(command) {
            error!("failed to send menu command: {err}");
        }
    }

    fn layout(&self) -> MenuLayout {
        let state = self.state.lock().unwrap().clone();
        let default_minutes = state.default_minutes;
        let has_session = state.session.is_some();
        let status_line = match &state.session {
            Some(session) => format!(
                "{} | {}m left | drift {}s",
                state.phase.label(),
                session.remaining.as_secs() / 60,
                session.bucket.as_secs()
            ),
            None => format!("Idle | default {default_minutes}m"),
        };

        let children = vec![
            menu_node(1, &format!("Start {default_minutes} min"), true),
            menu_node(2, "Start custom...", true),
            separator_node(3),
            menu_node(4, "Stop", has_session),
            menu_node(5, "Status notification", true),
            menu_node(6, &status_line, false),
            separator_node(7),
            menu_node(8, "Quit", true),
        ];

        (1, (0, HashMap::new(), children))
    }

    fn nodes(&self) -> Vec<MenuNodeData> {
        let state = self.state.lock().unwrap().clone();
        let default_minutes = state.default_minutes;
        let has_session = state.session.is_some();
        let status_line = match &state.session {
            Some(session) => format!(
                "{} | {}m left | drift {}s",
                state.phase.label(),
                session.remaining.as_secs() / 60,
                session.bucket.as_secs()
            ),
            None => format!("Idle | default {default_minutes}m"),
        };

        vec![
            menu_data(1, &format!("Start {default_minutes} min"), true),
            menu_data(2, "Start custom...", true),
            separator_data(3),
            menu_data(4, "Stop", has_session),
            menu_data(5, "Status notification", true),
            menu_data(6, &status_line, false),
            separator_data(7),
            menu_data(8, "Quit", true),
        ]
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl ClockstopMenu {
    fn about_to_show(&self, _id: i32) -> bool {
        true
    }

    fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }

        match id {
            1 => {
                let minutes = self.state.lock().unwrap().default_minutes;
                self.send(AppCommand::Start(Duration::from_secs(minutes * 60)));
            }
            2 => open_custom_picker(
                self.config.clone(),
                self.tx.clone(),
                self.state.clone(),
                self.picker_open.clone(),
            ),
            4 => self.send(AppCommand::Stop),
            5 => self.send(AppCommand::Status),
            8 => self.send(AppCommand::Quit),
            _ => {}
        }
    }

    fn get_layout(
        &self,
        _parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> MenuLayout {
        self.layout()
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> (u32, Vec<(i32, HashMap<String, OwnedValue>)>) {
        let nodes = self.nodes();
        let props = nodes
            .into_iter()
            .filter(|node| ids.is_empty() || ids.contains(&node.id))
            .map(|node| (node.id, node.fields))
            .collect();
        (1, props)
    }

    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        self.nodes()
            .into_iter()
            .find(|node| node.id == id)
            .and_then(|node| node.fields.into_iter().find(|(key, _)| key == name))
            .map(|(_, value)| value)
            .ok_or_else(|| zbus::fdo::Error::Failed(format!("unknown menu property {id}:{name}")))
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "normal".to_string()
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        0
    }
}

type MenuLayout = (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>));

struct MenuNodeData {
    id: i32,
    fields: HashMap<String, OwnedValue>,
}

fn menu_node(id: i32, label: &str, enabled: bool) -> OwnedValue {
    let data = menu_data(id, label, enabled);
    owned_node(data.id, data.fields)
}

fn menu_data(id: i32, label: &str, enabled: bool) -> MenuNodeData {
    let mut fields = HashMap::new();
    fields.insert("label".to_string(), owned_string(label));
    fields.insert("enabled".to_string(), owned_bool(enabled));
    fields.insert("visible".to_string(), owned_bool(true));
    MenuNodeData { id, fields }
}

fn separator_node(id: i32) -> OwnedValue {
    let data = separator_data(id);
    owned_node(data.id, data.fields)
}

fn separator_data(id: i32) -> MenuNodeData {
    let mut fields = HashMap::new();
    fields.insert("type".to_string(), owned_string("separator"));
    fields.insert("visible".to_string(), owned_bool(true));
    MenuNodeData { id, fields }
}

fn owned_node(id: i32, fields: HashMap<String, OwnedValue>) -> OwnedValue {
    OwnedValue::try_from(Value::from((id, fields, Vec::<OwnedValue>::new())))
        .expect("menu node serializes")
}

fn owned_string(value: &str) -> OwnedValue {
    OwnedValue::try_from(Value::from(value.to_string())).expect("menu string serializes")
}

fn owned_bool(value: bool) -> OwnedValue {
    OwnedValue::try_from(Value::from(value)).expect("menu bool serializes")
}

#[derive(Clone)]
struct TrayIcon {
    width: i32,
    height: i32,
    data: Vec<u8>,
}

fn solid_clock_icon(size: i32, (r, g, b): (u8, u8, u8)) -> TrayIcon {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.42;
    let inner = radius * 0.72;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let on_ring = dist <= radius && dist >= inner;
            let on_hand = (dx.abs() <= 1.0 && dy <= 1.0 && dy >= -inner)
                || (dy.abs() <= 1.0 && dx >= -1.0 && dx <= inner * 0.62);
            let (a, rr, gg, bb) = if on_ring || on_hand {
                (0xff, r, g, b)
            } else {
                (0x00, 0x00, 0x00, 0x00)
            };
            data.extend_from_slice(&[a, rr, gg, bb]);
        }
    }

    TrayIcon {
        width: size,
        height: size,
        data,
    }
}

#[derive(Debug)]
struct Session {
    duration: Duration,
    started_at: Instant,
    snapshot: Population,
    bucket: Duration,
    last_tick: Instant,
    warned: bool,
    locked_at: Option<Instant>,
    cooldown_until: Option<Instant>,
}

impl Session {
    fn view(&self, now: Instant, delta: Population) -> SessionView {
        let elapsed = now.saturating_duration_since(self.started_at);
        SessionView {
            duration: self.duration,
            remaining: self.duration.saturating_sub(elapsed),
            bucket: self.bucket,
            snapshot: self.snapshot.clone(),
            delta,
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clockstop=debug,info".parse().unwrap()),
        )
        .init();

    let config = Config::from_env();
    let tray_config = Arc::new(config.clone());
    let state = Arc::new(Mutex::new(SharedState::idle(config.default_minutes)));
    let picker_open = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();

    event(&config, &state, "clockstop ready");

    let tray_handle = spawn_tray_service(state.clone(), tray_config.clone(), tx.clone(), picker_open.clone());

    if std::env::args().any(|a| a == "--custom") {
        open_custom_picker(tray_config, tx.clone(), state.clone(), picker_open);
    }

    let exit = run_loop(config, state, rx, tray_handle);
    if let Err(err) = exit {
        error!("{err}");
        std::process::exit(1);
    }
}

fn spawn_tray_service(
    state: Arc<Mutex<SharedState>>,
    config: Arc<Config>,
    tx: Sender<AppCommand>,
    picker_open: Arc<AtomicBool>,
) -> TrayHandle {
    let (tray_tx, tray_rx) = mpsc::channel();
    let handle = TrayHandle { tx: tray_tx };

    thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(err) => {
                error!("failed to create tray runtime: {err}");
                return;
            }
        };

        runtime.block_on(async move {
            if let Err(err) = run_tray_service(state, config, tx, picker_open, tray_rx).await {
                error!("clockstop StatusNotifier tray service failed: {err}");
            }
        });
    });

    handle
}

async fn run_tray_service(
    state: Arc<Mutex<SharedState>>,
    config: Arc<Config>,
    tx: Sender<AppCommand>,
    picker_open: Arc<AtomicBool>,
    rx: Receiver<TraySignal>,
) -> zbus::Result<()> {
    let pid = std::process::id();
    let item_name = format!("org.kde.StatusNotifierItem-{pid}-1");
    let item = ClockstopItem {
        state: state.clone(),
        config: config.clone(),
        tx: tx.clone(),
        picker_open: picker_open.clone(),
    };
    let menu = ClockstopMenu {
        state,
        config,
        tx,
        picker_open,
    };

    info!("spawning clockstop zbus StatusNotifier tray service");
    let connection = zbus::connection::Builder::session()?
        .name(item_name.as_str())?
        .serve_at("/StatusNotifierItem", item.clone())?
        .serve_at("/MenuBar", menu)?
        .build()
        .await?;

    register_status_notifier(&connection, &item_name).await;

    loop {
        match rx.recv() {
            Ok(TraySignal::Update) => {
                emit_tray_update(&connection, &item).await;
            }
            Ok(TraySignal::Shutdown) | Err(_) => return Ok(()),
        }
    }
}

async fn register_status_notifier(connection: &zbus::Connection, item_name: &str) {
    for attempt in 1..=10 {
        let proxy = zbus::Proxy::new(
            connection,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .await;

        match proxy {
            Ok(proxy) => {
                let registered = proxy
                    .call_method("RegisterStatusNotifierItem", &(item_name))
                    .await;
                match registered {
                    Ok(_) => {
                        info!("registered StatusNotifier item via watcher call: {item_name}");
                        return;
                    }
                    Err(err) => warn!("StatusNotifier registration attempt {attempt} failed: {err}"),
                }
            }
            Err(err) => warn!("StatusNotifier watcher proxy attempt {attempt} failed: {err}"),
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    error!("failed to register StatusNotifier item after retries: {item_name}");
}

async fn emit_tray_update(connection: &zbus::Connection, item: &ClockstopItem) {
    let header = match zbus::object_server::SignalEmitter::new(connection, "/StatusNotifierItem") {
        Ok(header) => header,
        Err(err) => {
            warn!("failed to create tray signal emitter: {err}");
            return;
        }
    };

    if let Err(err) = ClockstopItem::new_title(&header).await {
        warn!("failed to emit tray title update: {err}");
    }
    if let Err(err) = ClockstopItem::new_status(&header, &item.status_text()).await {
        warn!("failed to emit tray status update: {err}");
    }
    if let Err(err) = ClockstopItem::new_icon(&header).await {
        warn!("failed to emit tray icon update: {err}");
    }
}

fn run_loop(
    config: Config,
    state: Arc<Mutex<SharedState>>,
    rx: Receiver<AppCommand>,
    tray_handle: TrayHandle,
) -> Result<(), String> {
    let mut session: Option<Session> = None;
    let mut last_delta = Population::new();

    loop {
        while let Ok(command) = rx.try_recv() {
            match command {
                AppCommand::Start(duration) => {
                    let snapshot = sample(&config)?;
                    let minutes = (duration.as_secs() / 60).max(1);
                    session = Some(Session {
                        duration,
                        started_at: Instant::now(),
                        snapshot,
                        bucket: Duration::ZERO,
                        last_tick: Instant::now(),
                        warned: false,
                        locked_at: None,
                        cooldown_until: None,
                    });
                    {
                        let mut shared = state.lock().unwrap();
                        shared.default_minutes = minutes;
                    }
                    update_shared(&state, Phase::Running, &session, last_delta.clone());
                    event(&config, &state, &format!("started {minutes}m session"));
                    notify("clockstop", &format!("started {minutes}m session"));
                    refresh_tray(&tray_handle);
                }
                AppCommand::Stop => {
                    session = None;
                    last_delta.clear();
                    update_shared(&state, Phase::Idle, &session, last_delta.clone());
                    event(&config, &state, "session stopped");
                    notify("clockstop", "session stopped");
                    refresh_tray(&tray_handle);
                }
                AppCommand::Status => {
                    let msg = status_message(&state);
                    event(&config, &state, &msg);
                    notify("clockstop status", &msg);
                    refresh_tray(&tray_handle);
                }
                AppCommand::Quit => {
                    event(&config, &state, "quitting");
                    tray_handle.shutdown();
                    return Ok(());
                }
            }
        }

        if let Some(active) = session.as_mut() {
            let now = Instant::now();
            if now.saturating_duration_since(active.last_tick) >= config.tick {
                let dt = now.saturating_duration_since(active.last_tick);
                active.last_tick = now;

                let current = sample(&config)?;
                last_delta = population_delta(&active.snapshot, &current);
                let drifting = !last_delta.is_empty();

                if active.locked_at.is_some() {
                    active.bucket = active.bucket.saturating_sub(dt);
                    if active.cooldown_until.is_none() {
                        active.cooldown_until = Some(now + config.threshold);
                        event(&config, &state, "unlock assumed; cooldown started");
                        notify("clockstop", "cooldown: close the drift window");
                    }
                } else if active.cooldown_until.is_some_and(|until| now < until) {
                    active.bucket = active.bucket.saturating_sub(dt);
                } else {
                    active.cooldown_until = None;
                    if drifting {
                        active.bucket = active.bucket.saturating_add(dt);
                    } else {
                        active.bucket = active.bucket.saturating_sub(dt);
                        active.warned = false;
                    }
                }

                let phase = if active.locked_at.is_some() {
                    Phase::Cooldown
                } else if active.cooldown_until.is_some() {
                    Phase::Cooldown
                } else if drifting && active.bucket >= config.threshold {
                    Phase::Drift
                } else {
                    Phase::Running
                };

                if drifting
                    && active.bucket >= config.threshold
                    && !active.warned
                    && active.locked_at.is_none()
                {
                    active.warned = true;
                    event(
                        &config,
                        &state,
                        &format!("drift warning: {}", delta_text(&last_delta)),
                    );
                    notify(
                        "clockstop drift",
                        &format!("close drift window: {}", delta_text(&last_delta)),
                    );
                }

                if drifting
                    && active.bucket >= config.threshold + config.threshold
                    && active.locked_at.is_none()
                {
                    event(&config, &state, "lock command fired");
                    notify("clockstop", "locking");
                    {
                        let mut shared = state.lock().unwrap();
                        shared.phase = Phase::Locked;
                        shared.session = Some(active.view(now, last_delta.clone()));
                    }
                    refresh_tray(&tray_handle);
                    run_lock_command(&config);
                    active.locked_at = Some(now);
                    active.cooldown_until = Some(now + config.threshold);
                }

                if now.saturating_duration_since(active.started_at) >= active.duration {
                    let minutes = active.duration.as_secs() / 60;
                    session = None;
                    last_delta.clear();
                    update_shared(&state, Phase::Idle, &session, last_delta.clone());
                    event(&config, &state, &format!("session {minutes}m complete"));
                    notify(
                        "clockstop",
                        &format!("clockstop session ({minutes}m) complete"),
                    );
                    refresh_tray(&tray_handle);
                } else {
                    update_shared(&state, phase, &session, last_delta.clone());
                    refresh_tray(&tray_handle);
                }
            }
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn update_shared(
    state: &Arc<Mutex<SharedState>>,
    phase: Phase,
    session: &Option<Session>,
    delta: Population,
) {
    let mut shared = state.lock().unwrap();
    shared.phase = phase;
    shared.session = session.as_ref().map(|s| s.view(Instant::now(), delta));
}

fn refresh_tray(handle: &TrayHandle) {
    handle.update();
}

fn sample(config: &Config) -> Result<Population, String> {
    if let Ok(raw) = std::env::var("CLOCKSTOP_SAMPLE_TEXT") {
        return Ok(parse_population(&raw));
    }

    let output = ProcessCommand::new("sh")
        .arg("-c")
        .arg(&config.sample_cmd)
        .output()
        .map_err(|err| format!("sample command failed to start: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "sample command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(parse_population(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_population(raw: &str) -> Population {
    let mut population = Population::new();
    let mut seen = BTreeMap::<String, u32>::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(base) = window_identity(line) {
            let ordinal = seen.entry(base.clone()).or_insert(0);
            *ordinal += 1;
            let identity = if *ordinal == 1 {
                base
            } else {
                format!("{base}#{ordinal}")
            };
            population.insert(identity, 1);
        }
    }
    population
}

fn window_identity(line: &str) -> Option<String> {
    let app_id = extract_app_id(line)?;
    let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(format!("{app_id}\t{normalized}"))
}

fn identity_app(identity: &str) -> &str {
    identity.split('\t').next().unwrap_or(identity)
}

fn extract_app_id(line: &str) -> Option<String> {
    for key in ["app_id", "app-id", "appid", "app"] {
        if let Some(value) = extract_key_value(line, key) {
            return Some(value);
        }
    }

    let first = line
        .split_whitespace()
        .next()?
        .trim_matches(|c| matches!(c, '"' | '\'' | '[' | ']' | '(' | ')' | ',' | ';'));
    if first.is_empty() || first.chars().all(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(first.to_string())
    }
}

fn extract_key_value(line: &str, key: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let start = lower.find(key)?;
    let after_key = &line[start + key.len()..];
    let after_sep = after_key
        .trim_start()
        .strip_prefix(['=', ':'])
        .unwrap_or(after_key)
        .trim_start();

    if after_sep.is_empty() {
        return None;
    }

    if let Some(rest) = after_sep.strip_prefix('"') {
        return rest.split('"').next().map(str::to_string);
    }

    if let Some(rest) = after_sep.strip_prefix('\'') {
        return rest.split('\'').next().map(str::to_string);
    }

    after_sep
        .split_whitespace()
        .next()
        .map(|s| s.trim_matches(',').to_string())
}

fn population_delta(snapshot: &Population, current: &Population) -> Population {
    let mut delta = Population::new();
    let keys: BTreeSet<_> = snapshot.keys().chain(current.keys()).collect();
    for key in keys {
        let before = snapshot.get(key).copied().unwrap_or(0);
        let now = current.get(key).copied().unwrap_or(0);
        if now > before {
            delta.insert(key.clone(), now - before);
        }
    }
    delta
}

fn delta_text(delta: &Population) -> String {
    if delta.is_empty() {
        return "none".to_string();
    }
    let mut grouped = BTreeMap::<String, u32>::new();
    for (identity, count) in delta {
        *grouped.entry(identity_app(identity).to_string()).or_insert(0) += count;
    }
    grouped
        .iter()
        .map(|(app, count)| format!("{app}+{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn status_message(state: &Arc<Mutex<SharedState>>) -> String {
    let shared = state.lock().unwrap();
    match &shared.session {
        Some(session) => format!(
            "{}: {}s left, drift {}s/{}, delta {}, snapshot {} windows",
            shared.phase.label(),
            session.remaining.as_secs(),
            session.bucket.as_secs(),
            session.duration.as_secs(),
            delta_text(&session.delta),
            session.snapshot.len()
        ),
        None => "idle".to_string(),
    }
}

fn event(config: &Config, state: &Arc<Mutex<SharedState>>, message: &str) {
    info!("{message}");
    {
        let mut shared = state.lock().unwrap();
        shared.last_event = message.to_string();
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.log_path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn notify(summary: &str, body: &str) {
    #[cfg(not(debug_assertions))]
    {
        let _ = (summary, body);
    }

    #[cfg(debug_assertions)]
    {
    debug!("notify: {summary}: {body}");
    if let Err(err) = ProcessCommand::new("notify-send")
        .arg(summary)
        .arg(body)
        .spawn()
    {
        warn!("notify-send failed: {err}");
    }
    }
}

fn run_lock_command(config: &Config) {
    if let Err(err) = ProcessCommand::new("sh")
        .arg("-c")
        .arg(&config.lock_cmd)
        .status()
    {
        error!("lock command failed: {err}");
    }
}

fn open_custom_picker(
    config: Arc<Config>,
    tx: Sender<AppCommand>,
    state: Arc<Mutex<SharedState>>,
    picker_open: Arc<AtomicBool>,
) {
    if picker_open.swap(true, Ordering::AcqRel) {
        return;
    }

    thread::spawn(move || {
        let default_minutes = state.lock().unwrap().default_minutes;
        let choices = format!("{default_minutes}\n15\n25\n45\n60\n90\n");
        let output = ProcessCommand::new("sh")
            .arg("-c")
            .arg(&config.picker_cmd)
            .env("CLOCKSTOP_DEFAULT_MINUTES", default_minutes.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(choices.as_bytes())?;
                }
                child.wait_with_output()
            });

        match output {
            Ok(output) if output.status.success() => {
                let picked = String::from_utf8_lossy(&output.stdout);
                match picked.trim().parse::<u64>() {
                    Ok(minutes) if (1..=480).contains(&minutes) => {
                        let _ = tx.send(AppCommand::Start(Duration::from_secs(minutes * 60)));
                    }
                    _ => notify("clockstop", "custom session needs 1-480 minutes"),
                }
            }
            Ok(_) => {}
            Err(err) => {
                error!("custom picker failed: {err}");
                notify("clockstop", "custom picker failed; check CLOCKSTOP_PICKER_CMD");
            }
        }

        picker_open.store(false, Ordering::Release);
    });
}
