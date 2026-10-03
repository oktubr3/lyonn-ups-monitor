//! App de barra de menú y tablero.
//!
//! Todo lo que toca AppKit (ícono de la barra, menú, ventana) vive en el hilo
//! principal; el hilo de sondeo solo publica en `Shared` y pide un repintado.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Layout, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, Ui, ViewportBuilder, ViewportCommand, pos2, vec2,
};
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR1};
use signal_hook::iterator::Signals;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

use crate::chart::{Chart, Series, day_label, format_ts, local_offset};
use crate::model::{
    Band, GridState, Limits, TOL_FREQ_HZ, battery_pct, format_duration, load_watts, num,
    runtime_minutes,
};
use crate::monitor::{self, Live, Shared, unix_now};
use crate::store::{Event, EventKind, Point, Stats, Store, data_dir};
use crate::theme::{self, font};

const HEAT_DAYS: i64 = 14;
const HEAT_BUCKET: i64 = 600;
const HEAT_COLS: i64 = 86_400 / HEAT_BUCKET;
const HEAT_REFRESH: Duration = Duration::from_secs(60);
const MAX_EVENTS: i64 = 200;

/// Opciones de arranque de la app de barra de menú.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Abrir el tablero al iniciar en vez de quedar solo en la barra.
    show: bool,
    range: Range,
    /// Escala de la interfaz (1.0 = tamaño normal).
    zoom: f32,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            show: false,
            range: Range::H1,
            zoom: 1.0,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("falta el valor de {arg}"));
            match arg.as_str() {
                "--show" => options.show = true,
                "--range" => {
                    let key = value()?;
                    options.range = Range::ALL
                        .into_iter()
                        .find(|r| r.key() == key)
                        .ok_or(format!("período desconocido: {key}"))?;
                }
                "--zoom" => {
                    let zoom = value()?;
                    options.zoom = zoom
                        .parse()
                        .ok()
                        .filter(|z| (0.3..=3.0).contains(z))
                        .ok_or(format!("zoom inválido: {zoom} (de 0.3 a 3)"))?;
                }
                other => return Err(format!("opción desconocida: {other}")),
            }
        }
        Ok(options)
    }
}

pub fn run(options: Options) -> Result<(), String> {
    let Options { show, .. } = options;
    // Una sola instancia: el UPS no admite dos procesos leyendo a la vez.
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("app.lock"))
        .map_err(|e| e.to_string())?;
    if lock.try_lock().is_err() {
        if !show {
            return Err("ups-monitor ya está corriendo (mirá la barra de menú).".into());
        }
        // `--show` con la app en marcha: se le pide que abra su tablero.
        let mut pid = String::new();
        lock.read_to_string(&mut pid).map_err(|e| e.to_string())?;
        let pid: libc::pid_t = pid
            .trim()
            .parse()
            .map_err(|_| "ups-monitor ya está corriendo, pero no se pudo ubicar su proceso.")?;
        // SAFETY: `kill` solo envía una señal; no accede a memoria de este proceso.
        if unsafe { libc::kill(pid, SIGUSR1) } != 0 {
            return Err(format!("no se pudo avisar a ups-monitor (pid {pid})."));
        }
        println!("Tablero abierto en la app que ya estaba corriendo.");
        return Ok(());
    }
    lock.set_len(0).map_err(|e| e.to_string())?;
    write!(lock, "{}", std::process::id()).map_err(|e| e.to_string())?;

    let native = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("UPS — Red eléctrica")
            .with_inner_size([1180.0, 900.0])
            .with_min_inner_size([940.0, 560.0])
            .with_visible(show),
        // Sin ícono en el Dock: la app vive en la barra de menú.
        event_loop_builder: Some(Box::new(|builder| {
            builder.with_activation_policy(ActivationPolicy::Accessory);
        })),
        ..Default::default()
    };
    eframe::run_native(
        "ups-monitor",
        native,
        Box::new(move |cc| Ok(Box::new(App::new(cc, options)))),
    )
    .map_err(|e| e.to_string())?;
    drop(lock);
    Ok(())
}

enum MenuAction {
    Open,
    Quit,
}

/// Ícono de la barra de menú con su menú de estado.
struct Tray {
    icon: TrayIcon,
    title: String,
    lines: [(MenuItem, String); 6],
}

impl Tray {
    fn new(ctx: &egui::Context, tx: Sender<MenuAction>) -> Result<Self, String> {
        let lines: [(MenuItem, String); 6] =
            std::array::from_fn(|_| (MenuItem::new("", false, None), String::new()));
        let open = MenuItem::new("Abrir tablero", true, None);
        let quit = MenuItem::new("Salir", true, None);

        let menu = Menu::new();
        let build = || -> tray_icon::menu::Result<()> {
            menu.append(&lines[0].0)?;
            menu.append(&PredefinedMenuItem::separator())?;
            for (item, _) in &lines[1..] {
                menu.append(item)?;
            }
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&open)?;
            menu.append(&quit)
        };
        build().map_err(|e| e.to_string())?;

        let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
        let ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = if event.id == open_id {
                MenuAction::Open
            } else if event.id == quit_id {
                MenuAction::Quit
            } else {
                return;
            };
            tx.send(action).ok();
            ctx.request_repaint();
        }));

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_title("⚡ —")
            .with_tooltip("UPS — Red eléctrica")
            .build()
            .map_err(|e| e.to_string())?;
        let mut tray = Self {
            icon,
            title: String::new(),
            lines,
        };
        tray.set_line(0, "Conectando…".into());
        Ok(tray)
    }

    fn set_title(&mut self, title: String) {
        if self.title != title {
            self.icon.set_title(Some(&title));
            self.title = title;
        }
    }

    fn set_line(&mut self, i: usize, text: String) {
        let (item, current) = &mut self.lines[i];
        if *current != text {
            item.set_text(&text);
            *current = text;
        }
    }

    fn update(&mut self, live: &Live, state: GridState, limits: &Limits, smooth_v: Option<f32>) {
        let Some(s) = live.status.filter(|_| live.connected) else {
            self.set_title("⚡ ?".into());
            self.set_line(0, GridState::Disconnected.title().into());
            for i in 1..self.lines.len() {
                self.set_line(i, String::new());
            }
            return;
        };
        let pct = battery_pct(&s, &live.rating);
        let title = if s.flags.on_battery() {
            format!("{} {pct}%", state.icon())
        } else {
            // Suavizada: la lectura cruda salta ±2 V a cada segundo.
            format!("{} {:.0} V", state.icon(), smooth_v.unwrap_or(s.input_v))
        };
        self.set_title(title);
        self.set_line(0, state.title().into());
        let input = if s.flags.on_battery() {
            "Entrada:  —".into()
        } else {
            format!(
                "Entrada:  {} V  ({} %)",
                num(s.input_v, 1),
                signed(limits.deviation_pct(s.input_v), 1)
            )
        };
        self.set_line(1, input);
        self.set_line(2, format!("Salida:  {} V", num(s.output_v, 1)));
        self.set_line(3, format!("Frecuencia:  {} Hz", num(s.freq_hz, 1)));
        self.set_line(4, format!("Carga:  {} %", s.load_pct));
        self.set_line(5, format!("Batería:  {pct} %  ({} V)", num(s.batt_v, 1)));
    }
}

fn signed(v: f32, decimals: usize) -> String {
    let sign = if v >= 0.0 { "+" } else { "−" };
    format!("{sign}{}", num(v.abs(), decimals))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Range {
    M15,
    H1,
    H6,
    H24,
    D7,
    D30,
}

impl Range {
    const ALL: [Range; 6] = [
        Range::M15,
        Range::H1,
        Range::H6,
        Range::H24,
        Range::D7,
        Range::D30,
    ];

    fn label(self) -> &'static str {
        match self {
            Range::M15 => "15 min",
            Range::H1 => "1 h",
            Range::H6 => "6 h",
            Range::H24 => "24 h",
            Range::D7 => "7 días",
            Range::D30 => "30 días",
        }
    }

    /// Nombre del período en la línea de comandos.
    fn key(self) -> &'static str {
        match self {
            Range::M15 => "15m",
            Range::H1 => "1h",
            Range::H6 => "6h",
            Range::H24 => "24h",
            Range::D7 => "7d",
            Range::D30 => "30d",
        }
    }

    fn secs(self) -> i64 {
        match self {
            Range::M15 => 900,
            Range::H1 => 3_600,
            Range::H6 => 21_600,
            Range::H24 => 86_400,
            Range::D7 => 7 * 86_400,
            Range::D30 => 30 * 86_400,
        }
    }

    /// Segundos por punto del gráfico.
    fn bucket(self) -> i64 {
        match self {
            Range::M15 => 1,
            Range::H1 => 5,
            Range::H6 => 30,
            Range::H24 => 120,
            Range::D7 => 900,
            Range::D30 => 3_600,
        }
    }

    /// Los rangos cortos leen las muestras crudas; el resto, los minutos.
    fn raw(self) -> bool {
        matches!(self, Range::M15 | Range::H1 | Range::H6)
    }
}

/// Datos del rango elegido, tal como se consultaron por última vez.
struct RangeData {
    range: Range,
    loaded: Instant,
    from: i64,
    to: i64,
    xs: Vec<i64>,
    points: Vec<Point>,
    stats: Stats,
    hist: Vec<(i64, i64)>,
    events: Vec<Event>,
    outages: Vec<(i64, i64)>,
}

impl RangeData {
    fn load(store: &Store, range: Range, limits: &Limits) -> rusqlite::Result<Self> {
        let to = unix_now();
        let from = to - range.secs();
        let (bucket, raw) = (range.bucket(), range.raw());
        let points = store.series(from, to, bucket, raw)?;
        let events = store.events(from, to, MAX_EVENTS)?;
        let outages = events
            .iter()
            .filter(|e| e.kind == EventKind::Outage)
            .map(|e| (e.ts_start, e.ts_end.unwrap_or(to)))
            .collect();
        Ok(Self {
            range,
            loaded: Instant::now(),
            from,
            to,
            xs: points.iter().map(|p| p.ts).collect(),
            stats: store.stats(from, to, raw, limits)?,
            hist: store.histogram(from, to, raw)?,
            points,
            events,
            outages,
        })
    }

    fn column(&self, f: impl Fn(&Point) -> Option<f64>) -> Vec<f64> {
        self.points
            .iter()
            .map(|p| f(p).unwrap_or(f64::NAN))
            .collect()
    }
}

/// Celdas del cuadro: una por cada 10 minutos de los últimos días.
struct Heat {
    loaded: Instant,
    /// Medianoche local del día más antiguo.
    day0: i64,
    cells: Vec<Option<Point>>,
    /// Extremos de la escala de color, en voltios.
    scale: (f64, f64),
}

/// Ancho mínimo de la escala: evita que el ruido de medición llene el cuadro
/// de contrastes que no significan nada.
const HEAT_MIN_SPAN_V: f64 = 8.0;

impl Heat {
    fn load(store: &Store, limits: &Limits) -> rusqlite::Result<Self> {
        let now = unix_now();
        let offset = local_offset(now);
        let today = now - (now + offset).rem_euclid(86_400);
        let day0 = today - (HEAT_DAYS - 1) * 86_400;
        let mut cells = vec![None; (HEAT_DAYS * HEAT_COLS) as usize];
        for p in store.series(day0, now, HEAT_BUCKET, false)? {
            let i = (p.ts - day0).div_euclid(HEAT_BUCKET);
            if let Some(cell) = usize::try_from(i).ok().and_then(|i| cells.get_mut(i)) {
                *cell = Some(p);
            }
        }
        let mut volts: Vec<f64> = cells.iter().flatten().filter_map(|p| p.vin_avg).collect();
        volts.sort_by(f64::total_cmp);
        let scale = heat_scale(&volts, f64::from(limits.nominal));
        Ok(Self {
            loaded: Instant::now(),
            day0,
            cells,
            scale,
        })
    }

    /// Un corte siempre se ve, por breve que sea; el resto se pinta según la
    /// tensión media de la celda.
    fn color(&self, p: &Point) -> Color32 {
        match p.vin_avg {
            _ if p.n_batt > 0 => theme::CRITICAL,
            Some(v) => theme::ramp(((v - self.scale.0) / (self.scale.1 - self.scale.0)) as f32),
            None => theme::GRID,
        }
    }
}

/// Escala a voltios enteros que abarca lo habitual (percentiles 2 a 98 de
/// `sorted`), con un ancho mínimo. Un bajón puntual satura el extremo en vez
/// de aplastar el resto del cuadro contra el otro.
fn heat_scale(sorted: &[f64], nominal: f64) -> (f64, f64) {
    let Some(last) = sorted.len().checked_sub(1) else {
        return (
            nominal - HEAT_MIN_SPAN_V / 2.0,
            nominal + HEAT_MIN_SPAN_V / 2.0,
        );
    };
    let (min, max) = (sorted[last * 2 / 100], sorted[last - last * 2 / 100]);
    let (mut lo, mut hi) = (min.floor(), max.ceil());
    let missing = HEAT_MIN_SPAN_V - (hi - lo);
    if missing > 0.0 {
        lo -= (missing / 2.0).floor();
        hi = lo + HEAT_MIN_SPAN_V;
    }
    (lo, hi)
}

struct App {
    shared: Shared,
    live: Live,
    limits: Limits,
    state: GridState,
    /// Desde cuándo rige `state`, si cambió con la app en marcha.
    state_since: Option<i64>,
    /// Tensión de entrada suavizada: el UPS mide en pasos de ~2 V y la
    /// lectura instantánea salta de banda a cada segundo.
    vin_smooth: Option<f32>,
    tray: Option<Tray>,
    menu_rx: Receiver<MenuAction>,
    quitting: bool,
    /// Cuadros que faltan para ocultar la ventana al arrancar sin `--show`.
    hide_in: Option<u8>,
    store: Option<Store>,
    range: Range,
    data: Option<RangeData>,
    heat: Option<Heat>,
    hover: Option<i64>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, options: Options) -> Self {
        let Options { show, range, zoom } = options;
        let ctx = &cc.egui_ctx;
        ctx.set_zoom_factor(zoom);
        ctx.set_visuals(egui::Visuals::dark());
        ctx.global_style_mut(|style| {
            style.visuals.panel_fill = theme::PLANE;
            style.visuals.window_fill = theme::SURFACE;
            style.visuals.override_text_color = Some(theme::INK_2);
            style.visuals.selection.bg_fill = theme::alpha(theme::VIN, 90);
            style.spacing.item_spacing = vec2(8.0, 8.0);
            style.spacing.button_padding = vec2(10.0, 4.0);
        });

        let shared: Shared = Arc::new(Mutex::new(Live::default()));
        let wake = ctx.clone();
        monitor::spawn(shared.clone(), move || wake.request_repaint());

        let (tx, menu_rx) = channel();
        // SIGUSR1 (lo envía `ups-monitor --show`) abre el tablero igual que el
        // menú; SIGTERM y SIGINT salen ordenadamente, igual que "Salir".
        if let Ok(mut signals) = Signals::new([SIGUSR1, SIGTERM, SIGINT]) {
            let (tx, wake) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                for signal in signals.forever() {
                    let action = if signal == SIGUSR1 {
                        MenuAction::Open
                    } else {
                        MenuAction::Quit
                    };
                    tx.send(action).ok();
                    wake.request_repaint();
                }
            });
        }

        let tray = match Tray::new(ctx, tx) {
            Ok(tray) => Some(tray),
            Err(e) => {
                crate::notify::log(&format!(
                    "ERROR: no se pudo crear el ícono de la barra: {e}"
                ));
                // Sin ícono no habría forma de abrir el tablero.
                if !show {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                }
                None
            }
        };

        Self {
            shared,
            live: Live::default(),
            limits: Limits::new(Live::default().rating.voltage),
            state: GridState::Disconnected,
            state_since: None,
            vin_smooth: None,
            menu_rx,
            quitting: false,
            // eframe muestra la ventana tras pintar el primer cuadro, sin
            // importar `with_visible`; se la oculta en el cuadro siguiente.
            hide_in: (!show && tray.is_some()).then_some(1),
            tray,
            store: Store::open().ok(),
            range,
            data: None,
            heat: None,
            hover: None,
        }
    }

    fn refresh_data(&mut self) {
        let Some(store) = &self.store else { return };
        // Cada rango se vuelve a consultar al ritmo de su resolución.
        let every = Duration::from_secs(self.range.bucket().clamp(1, 30) as u64);
        let stale = self
            .data
            .as_ref()
            .is_none_or(|d| d.range != self.range || d.loaded.elapsed() >= every);
        if stale && let Ok(data) = RangeData::load(store, self.range, &self.limits) {
            self.data = Some(data);
        }
        if self
            .heat
            .as_ref()
            .is_none_or(|h| h.loaded.elapsed() >= HEAT_REFRESH)
            && let Ok(heat) = Heat::load(store, &self.limits)
        {
            self.heat = Some(heat);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match self.hide_in {
            Some(0) => {
                self.hide_in = None;
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            Some(n) => {
                self.hide_in = Some(n - 1);
                ctx.request_repaint();
            }
            None => {}
        }

        while let Ok(action) = self.menu_rx.try_recv() {
            match action {
                MenuAction::Open => {
                    self.hide_in = None;
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                MenuAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }

        // Cerrar la ventana solo la oculta; se sale desde el menú.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting && self.tray.is_some() {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }

        let live = match self.shared.lock() {
            Ok(live) if live.generation != self.live.generation => live.clone(),
            _ => return,
        };
        self.limits = Limits::new(live.rating.voltage);
        let state = match live.status {
            Some(s) if live.connected => GridState::of(&s, &self.limits),
            _ => GridState::Disconnected,
        };
        if state != self.state {
            // El primer estado conocido no es un cambio.
            let first = self.live.generation == 0;
            self.state_since = (!first).then(unix_now);
            self.state = state;
        }
        self.vin_smooth = match live.status {
            Some(s) if live.connected && !s.flags.on_battery() => Some(
                self.vin_smooth
                    .map_or(s.input_v, |v| v + (s.input_v - v) * 0.1),
            ),
            _ => None,
        };
        if let Some(tray) = &mut self.tray {
            tray.update(&live, state, &self.limits, self.vin_smooth);
        }
        self.live = live;
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.refresh_data();
        // El reloj y las duraciones avanzan aunque no llegue una muestra.
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        Frame::new()
            .fill(theme::PLANE)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    self.header(ui);
                    self.flow(ui);
                    self.range_bar(ui);
                    let gap = ui.spacing().item_spacing.x;
                    let side = 330.0;
                    let main = (ui.available_width() - side - gap).max(300.0);
                    ui.horizontal_top(|ui| {
                        column(ui, main, |ui| self.charts(ui));
                        column(ui, side, |ui| {
                            self.quality(ui);
                            self.histogram(ui);
                            self.events(ui);
                        });
                    });
                    self.heatmap(ui);
                });
            });
    }
}

fn column(ui: &mut Ui, width: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        add(ui);
    });
}

fn card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::border()))
        .corner_radius(10.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.label(RichText::new(title).font(font(14.0)).color(theme::INK));
            }
            add(ui);
        });
}

fn key_value(ui: &mut Ui, key: &str, value: String) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(key).font(font(12.5)).color(theme::MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).font(font(12.5)).color(theme::INK));
        });
    });
}

fn volts(v: Option<f64>) -> String {
    v.map_or("—".into(), |v| format!("{} V", num(v, 1)))
}

fn percent(part: i64, total: i64) -> String {
    if total <= 0 {
        return "—".into();
    }
    let pct = part as f64 * 100.0 / total as f64;
    // Un valor chico pero no nulo no debe redondearse a cero ni a cien.
    let decimals = if part > 0 && part < total && !(1.0..=99.0).contains(&pct) {
        3
    } else {
        1
    };
    format!("{} %", num(pct, decimals))
}

fn event_color(kind: EventKind) -> Color32 {
    match kind {
        EventKind::Outage | EventKind::Failure | EventKind::BattLow => theme::CRITICAL,
        EventKind::Sag | EventKind::Swell => theme::SERIOUS,
        EventKind::Avr => theme::WARNING,
    }
}

impl App {
    fn header(&self, ui: &mut Ui) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
        let p = ui.painter_at(rect);
        let color = theme::severity(self.state.severity());
        let center = pos2(rect.left() + 12.0, rect.top() + 18.0);
        p.circle_filled(center, 11.0, theme::alpha(color, 50));
        p.circle_filled(center, 6.0, color);
        p.text(
            pos2(rect.left() + 34.0, rect.top() + 18.0),
            Align2::LEFT_CENTER,
            self.state.title(),
            font(24.0),
            theme::INK,
        );

        let subtitle = match (self.live.status, self.state_since) {
            (_, Some(since)) => format!(
                "Desde las {} (hace {})",
                format_ts(since, "%H:%M:%S"),
                format_duration(unix_now() - since)
            ),
            (Some(_), None) if self.live.connected => {
                "Sin cambios desde que inició el monitor".into()
            }
            _ => match &self.live.error {
                Some(e) => format!("Buscando el UPS por USB… ({e})"),
                None => "Buscando el UPS por USB…".into(),
            },
        };
        p.text(
            pos2(rect.left() + 34.0, rect.top() + 42.0),
            Align2::LEFT_CENTER,
            subtitle,
            font(12.5),
            theme::MUTED,
        );

        let r = self.live.rating;
        p.text(
            pos2(rect.right(), rect.top() + 14.0),
            Align2::RIGHT_CENTER,
            format!(
                "Lyonn CTB-800V  ·  nominal {} V / {} Hz",
                num(r.voltage, 0),
                num(r.freq_hz, 0)
            ),
            font(12.5),
            theme::INK_2,
        );
        let updated = if self.live.ts > 0 {
            format!("Última lectura {}", format_ts(self.live.ts, "%H:%M:%S"))
        } else {
            "Sin lecturas todavía".into()
        };
        p.text(
            pos2(rect.right(), rect.top() + 34.0),
            Align2::RIGHT_CENTER,
            updated,
            font(12.0),
            theme::MUTED,
        );
    }

    /// El camino de la energía: red → UPS → equipos, con la batería debajo.
    fn flow(&self, ui: &mut Ui) {
        card(ui, "", |ui| {
            const NODE_H: f32 = 92.0;
            const LINK_V: f32 = 30.0;
            let width = ui.available_width();
            let (rect, _) =
                ui.allocate_exact_size(vec2(width, NODE_H * 2.0 + LINK_V), Sense::hover());
            let p = ui.painter_at(rect);
            let link_w = (width * 0.07).clamp(40.0, 90.0);
            let node_w = (width - 2.0 * link_w) / 3.0;
            let node = |col: usize, row: usize| {
                let x = rect.left() + col as f32 * (node_w + link_w);
                let y = rect.top() + row as f32 * (NODE_H + LINK_V);
                Rect::from_min_size(pos2(x, y), vec2(node_w, NODE_H))
            };
            let (grid, ups, out, batt) = (node(0, 0), node(1, 0), node(2, 0), node(1, 1));

            let status = self.live.status.filter(|_| self.live.connected);
            let on_battery = status.is_some_and(|s| s.flags.on_battery());
            let state_color = theme::severity(self.state.severity());

            // Enlaces: color sólido donde circula energía, punteado donde no.
            let link = |a: egui::Pos2, b: egui::Pos2, live: bool, color: Color32| {
                if live {
                    p.line_segment([a, b], Stroke::new(3.0, color));
                } else {
                    p.extend(egui::Shape::dashed_line(
                        &[a, b],
                        Stroke::new(2.0, theme::AXIS),
                        5.0,
                        5.0,
                    ));
                }
            };
            let grid_color = if on_battery { theme::AXIS } else { state_color };
            link(
                grid.right_center(),
                ups.left_center(),
                status.is_some() && !on_battery,
                grid_color,
            );
            link(
                ups.right_center(),
                out.left_center(),
                status.is_some(),
                theme::VOUT,
            );
            link(
                ups.center_bottom(),
                batt.center_top(),
                on_battery,
                theme::BATT,
            );

            let draw = |r: Rect, accent: Color32, title: &str, value: String, sub: String| {
                p.rect_filled(r, CornerRadius::same(8), theme::PLANE);
                p.rect_stroke(
                    r,
                    CornerRadius::same(8),
                    Stroke::new(1.0, theme::border()),
                    StrokeKind::Inside,
                );
                p.rect_filled(
                    Rect::from_min_size(r.left_top() + vec2(0.0, 12.0), vec2(3.0, NODE_H - 24.0)),
                    0.0,
                    accent,
                );
                let x = r.left() + 16.0;
                p.text(
                    pos2(x, r.top() + 18.0),
                    Align2::LEFT_CENTER,
                    title,
                    font(11.5),
                    theme::MUTED,
                );
                p.text(
                    pos2(x, r.top() + 46.0),
                    Align2::LEFT_CENTER,
                    value,
                    font(26.0),
                    theme::INK,
                );
                p.text(
                    pos2(x, r.top() + 74.0),
                    Align2::LEFT_CENTER,
                    sub,
                    font(12.0),
                    theme::INK_2,
                );
            };

            let Some(s) = status else {
                for (r, title) in [
                    (grid, "RED ELÉCTRICA"),
                    (ups, "UPS"),
                    (out, "EQUIPOS"),
                    (batt, "BATERÍA"),
                ] {
                    draw(r, theme::AXIS, title, "—".into(), "Sin datos".into());
                }
                return;
            };
            let rating = &self.live.rating;

            if on_battery {
                draw(
                    grid,
                    theme::CRITICAL,
                    "RED ELÉCTRICA",
                    "Sin tensión".into(),
                    "Corte de luz".into(),
                );
            } else {
                // Valor, desvío y banda salen de la misma tensión suavizada
                // para que no se contradigan; la lectura cruda está en el gráfico.
                let vin = self.vin_smooth.unwrap_or(s.input_v);
                let band = self.limits.band(vin);
                draw(
                    grid,
                    theme::band(band),
                    "RED ELÉCTRICA",
                    format!("{} V", num(vin, 1)),
                    format!(
                        "{} % · {} Hz · {}",
                        signed(self.limits.deviation_pct(vin), 1),
                        num(s.freq_hz, 1),
                        band.label()
                    ),
                );
            }

            let mode = if s.flags.ups_failed() {
                "Falla"
            } else if on_battery {
                "En batería"
            } else if s.flags.avr_active() {
                "Regulando (AVR)"
            } else {
                "En línea"
            };
            draw(
                ups,
                state_color,
                "UPS",
                mode.into(),
                format!("Última transferencia a {} V", num(s.fault_v, 0)),
            );

            draw(
                out,
                theme::VOUT,
                "EQUIPOS",
                format!("{} V", num(s.output_v, 1)),
                format!(
                    "Carga {} % · ~{} W",
                    s.load_pct,
                    num(load_watts(s.load_pct), 0)
                ),
            );

            let pct = battery_pct(&s, rating);
            let accent = if s.flags.batt_low() || pct < 25 {
                theme::CRITICAL
            } else if pct < 60 {
                theme::WARNING
            } else {
                theme::GOOD
            };
            let runtime = runtime_minutes(&s, rating)
                .map_or("autonomía sin estimar (carga 0 %)".into(), |m| {
                    format!("autonomía ~{m} min")
                });
            let mode = if on_battery {
                "descargando"
            } else {
                "en flote"
            };
            draw(
                batt,
                accent,
                "BATERÍA (ESTIMADA)",
                format!("{pct} %"),
                format!("{} V · {mode} · {runtime}", num(s.batt_v, 1)),
            );
        });
    }

    fn range_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Período")
                    .font(font(12.5))
                    .color(theme::MUTED),
            );
            for range in Range::ALL {
                ui.selectable_value(&mut self.range, range, range.label());
            }
            if let Some(d) = &self.data
                && d.range == self.range
            {
                let coverage = d.stats.n as f64 * 100.0 / self.range.secs() as f64;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "Datos registrados: {} % del período",
                            num(coverage.min(100.0), 0)
                        ))
                        .font(font(12.0))
                        .color(theme::MUTED),
                    );
                });
            }
        });
        // El rango se recarga ya, sin esperar al próximo ciclo.
        if self.data.as_ref().is_some_and(|d| d.range != self.range) {
            self.refresh_data();
        }
    }

    fn charts(&mut self, ui: &mut Ui) {
        let Some(d) = &self.data else {
            card(ui, "Sin historial todavía", |_| {});
            return;
        };
        let l = self.limits;
        let nominal_hz = f64::from(self.live.rating.freq_hz);
        let (lo, hi) = (f64::from(l.good_lo), f64::from(l.good_hi));
        let (norm_lo, norm_hi) = (f64::from(l.norm_lo), f64::from(l.norm_hi));
        let base = |title, unit, decimals, height| Chart {
            title,
            unit,
            decimals,
            from: d.from,
            to: d.to,
            bucket: d.range.bucket(),
            xs: &d.xs,
            series: Vec::new(),
            y_include: Vec::new(),
            y_min_span: 0.0,
            bands: Vec::new(),
            refs: Vec::new(),
            spans: &d.outages,
            height,
        };
        let hover = self.hover;
        let mut next_hover = None;

        card(ui, "", |ui| {
            let mut tension = base("Tensión", "V", 1, 230.0);
            tension.series = vec![
                Series {
                    name: "Entrada",
                    color: theme::VIN,
                    ys: d.column(|p| p.vin_avg),
                    envelope: (d.range.bucket() > 1)
                        .then(|| (d.column(|p| p.vin_min), d.column(|p| p.vin_max))),
                },
                Series {
                    name: "Salida",
                    color: theme::VOUT,
                    ys: d.column(|p| Some(p.vout_avg)),
                    envelope: None,
                },
            ];
            tension.y_include = vec![lo, hi];
            tension.bands = vec![
                (lo, hi, theme::alpha(theme::GOOD, 22)),
                (norm_lo, lo, theme::alpha(theme::WARNING, 16)),
                (hi, norm_hi, theme::alpha(theme::WARNING, 16)),
            ];
            tension.refs = vec![f64::from(l.nominal), norm_lo, norm_hi];
            next_hover = next_hover.or(tension.show(ui, hover));
            ui.label(
                RichText::new(format!(
                    "Franja verde: óptima (±5 % de {nominal} V) · amarilla: tolerable (±8 %) · \
                     rojo: corte de luz",
                    nominal = num(l.nominal, 0)
                ))
                .font(font(11.0))
                .color(theme::MUTED),
            );
            ui.add_space(6.0);

            let mut freq = base("Frecuencia", "Hz", 2, 130.0);
            freq.series = vec![Series {
                name: "Frecuencia",
                color: theme::FREQ,
                ys: d.column(|p| p.freq_avg),
                envelope: None,
            }];
            let tol = f64::from(TOL_FREQ_HZ);
            freq.y_include = vec![nominal_hz - tol, nominal_hz + tol];
            freq.bands = vec![(
                nominal_hz - tol,
                nominal_hz + tol,
                theme::alpha(theme::GOOD, 22),
            )];
            freq.refs = vec![nominal_hz];
            next_hover = next_hover.or(freq.show(ui, hover));
            ui.add_space(6.0);

            let mut batt = base("Batería", "V", 1, 130.0);
            batt.series = vec![Series {
                name: "Batería",
                color: theme::BATT,
                ys: d.column(|p| Some(p.batt_min)),
                envelope: None,
            }];
            batt.y_min_span = 1.0;
            next_hover = next_hover.or(batt.show(ui, hover));
            ui.add_space(6.0);

            let mut load = base("Carga", "%", 0, 130.0);
            load.series = vec![Series {
                name: "Carga",
                color: theme::LOAD,
                ys: d.column(|p| Some(p.load_avg)),
                envelope: None,
            }];
            load.y_include = vec![0.0];
            load.y_min_span = 10.0;
            next_hover = next_hover.or(load.show(ui, hover));
        });
        self.hover = next_hover;
    }

    fn quality(&self, ui: &mut Ui) {
        card(ui, "Calidad de la red", |ui| {
            let Some(d) = &self.data else { return };
            let s = &d.stats;
            if s.n == 0 {
                ui.label(RichText::new("Sin datos en el período").color(theme::MUTED));
                return;
            }
            let parts = [
                (Band::Good, s.n_good),
                (Band::Marginal, s.n_marginal),
                (Band::Out, s.n_out),
                (Band::Outage, s.n_batt),
            ];

            // Barra apilada del tiempo observado en cada banda.
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
            let p = ui.painter_at(rect);
            let mut x = rect.left();
            for (band, n) in parts {
                if n == 0 {
                    continue;
                }
                // Un tramo mínimo visible: un corte breve no debe desaparecer.
                let w = (rect.width() * n as f32 / s.n as f32).max(3.0);
                let seg = Rect::from_min_max(
                    pos2(x, rect.top()),
                    pos2((x + w).min(rect.right()), rect.bottom()),
                );
                p.rect_filled(seg.shrink2(vec2(1.0, 0.0)), 3.0, theme::band(band));
                x += w;
            }

            for (band, n) in parts {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, theme::band(band));
                    ui.label(
                        RichText::new(band.label())
                            .font(font(12.5))
                            .color(theme::INK_2),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(percent(n, s.n))
                                .font(font(12.5))
                                .color(theme::INK),
                        );
                        ui.label(
                            RichText::new(format_duration(n))
                                .font(font(12.0))
                                .color(theme::MUTED),
                        );
                    });
                });
            }
            ui.separator();

            let outages: Vec<&Event> = d
                .events
                .iter()
                .filter(|e| e.kind == EventKind::Outage)
                .collect();
            let longest = outages
                .iter()
                .map(|e| e.ts_end.unwrap_or(d.to) - e.ts_start)
                .max();
            key_value(ui, "Disponibilidad de la red", percent(s.n_grid(), s.n));
            key_value(
                ui,
                "Cortes de luz",
                match longest {
                    Some(longest) => format!(
                        "{} · el más largo {}",
                        outages.len(),
                        format_duration(longest)
                    ),
                    None => "ninguno".into(),
                },
            );
            key_value(ui, "Tiempo con AVR activo", format_duration(s.n_avr));
            ui.separator();
            key_value(
                ui,
                "Tensión mín / media / máx",
                format!(
                    "{} / {} / {}",
                    s.vin_min.map_or("—".into(), |v| num(v, 1)),
                    s.vin_avg.map_or("—".into(), |v| num(v, 1)),
                    volts(s.vin_max)
                ),
            );
            key_value(
                ui,
                "Frecuencia mín / máx",
                match (s.freq_min, s.freq_max) {
                    (Some(lo), Some(hi)) => format!("{} / {} Hz", num(lo, 1), num(hi, 1)),
                    _ => "—".into(),
                },
            );
            key_value(ui, "Batería mínima", volts(s.batt_min));
            key_value(
                ui,
                "Carga media / máx",
                match (s.load_avg, s.load_max) {
                    (Some(avg), Some(max)) => format!("{} / {} %", num(avg, 0), num(max, 0)),
                    _ => "—".into(),
                },
            );
        });
    }

    fn histogram(&self, ui: &mut Ui) {
        card(ui, "Distribución de la tensión de entrada", |ui| {
            let Some(d) = &self.data else { return };
            let (Some(&(v_min, _)), Some(&(v_max, _))) = (d.hist.first(), d.hist.last()) else {
                ui.label(RichText::new("Sin datos en el período").color(theme::MUTED));
                return;
            };
            // Siempre se ve al menos la franja óptima para dar contexto.
            let lo = v_min.min(self.limits.good_lo.floor() as i64) - 1;
            let hi = v_max.max(self.limits.good_hi.ceil() as i64) + 1;
            let total: i64 = d.hist.iter().map(|&(_, n)| n).sum();
            let peak = d.hist.iter().map(|&(_, n)| n).max().unwrap_or(1).max(1);

            let (rect, response) =
                ui.allocate_exact_size(vec2(ui.available_width(), 120.0), Sense::hover());
            let p = ui.painter_at(rect);
            let plot = Rect::from_min_max(
                rect.left_top() + vec2(0.0, 6.0),
                rect.right_bottom() - vec2(0.0, 18.0),
            );
            let bin_w = plot.width() / (hi - lo + 1) as f32;
            let x_of = |v: f64| plot.left() + (v - lo as f64 + 0.5) as f32 * bin_w;

            let good = Rect::from_min_max(
                pos2(x_of(f64::from(self.limits.good_lo)), plot.top()),
                pos2(x_of(f64::from(self.limits.good_hi)), plot.bottom()),
            );
            p.rect_filled(good, 0.0, theme::alpha(theme::GOOD, 22));
            p.hline(plot.x_range(), plot.bottom(), Stroke::new(1.0, theme::AXIS));

            let hovered = response
                .hover_pos()
                .map(|pos| lo + ((pos.x - plot.left()) / bin_w).floor() as i64);
            for &(v, n) in &d.hist {
                let h = (plot.height() * n as f32 / peak as f32).max(1.5);
                let x = x_of(v as f64);
                let gap = if bin_w > 5.0 { 1.0 } else { 0.0 };
                let bar = Rect::from_min_max(
                    pos2(x - bin_w / 2.0 + gap, plot.bottom() - h),
                    pos2(x + bin_w / 2.0 - gap, plot.bottom()),
                );
                let color = if hovered == Some(v) {
                    theme::INK
                } else {
                    theme::VIN
                };
                p.rect_filled(
                    bar,
                    CornerRadius {
                        nw: 2,
                        ne: 2,
                        sw: 0,
                        se: 0,
                    },
                    color,
                );
            }

            let nominal = f64::from(self.limits.nominal);
            for (v, align) in [
                (lo as f64, Align2::LEFT_TOP),
                (nominal, Align2::CENTER_TOP),
                (hi as f64, Align2::RIGHT_TOP),
            ] {
                let x = x_of(v).clamp(plot.left(), plot.right());
                p.text(
                    pos2(x, plot.bottom() + 4.0),
                    align,
                    format!("{} V", num(v, 0)),
                    font(10.5),
                    theme::MUTED,
                );
            }

            if let Some(v) = hovered
                && let Some(&(_, n)) = d.hist.iter().find(|&&(bin, _)| bin == v)
            {
                response.on_hover_text_at_pointer(format!(
                    "{v} V — {} ({})",
                    format_duration(n),
                    percent(n, total)
                ));
            }
        });
    }

    fn events(&self, ui: &mut Ui) {
        card(ui, "Eventos", |ui| {
            let Some(d) = &self.data else { return };
            if d.events.is_empty() {
                ui.label(
                    RichText::new("Sin eventos en el período")
                        .font(font(12.5))
                        .color(theme::MUTED),
                );
                return;
            }
            const SHOWN: usize = 8;
            for e in d.events.iter().take(SHOWN) {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 4.0, event_color(e.kind));
                    ui.label(
                        RichText::new(e.kind.label())
                            .font(font(12.5))
                            .color(theme::INK),
                    );
                    ui.label(
                        RichText::new(format_ts(e.ts_start, "%d/%m %H:%M:%S"))
                            .font(font(12.0))
                            .color(theme::MUTED),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let duration = match e.ts_end {
                            Some(end) => format_duration(end - e.ts_start),
                            None => "en curso".into(),
                        };
                        ui.label(RichText::new(duration).font(font(12.5)).color(theme::INK_2));
                    })
                    .response
                    .on_hover_text(match e.value {
                        Some(v) => format!("{} {} V", e.kind.value_label(), num(v, 1)),
                        None => e.kind.label().into(),
                    });
                });
            }
            if d.events.len() > SHOWN {
                ui.label(
                    RichText::new(format!(
                        "y {} más — `ups-monitor events` los lista todos",
                        d.events.len() - SHOWN
                    ))
                    .font(font(11.5))
                    .color(theme::MUTED),
                );
            }
        });
    }

    /// El cuadro: cada fila es un día, cada celda diez minutos de red.
    fn heatmap(&self, ui: &mut Ui) {
        card(
            ui,
            "El cuadro de la red — tensión de entrada, últimos 14 días",
            |ui| {
                let Some(heat) = &self.heat else { return };
                const LABEL_W: f32 = 76.0;
                const ROW_H: f32 = 16.0;
                const TOP: f32 = 16.0;
                let width = ui.available_width();
                let (rect, response) = ui.allocate_exact_size(
                    vec2(width, TOP + ROW_H * HEAT_DAYS as f32),
                    Sense::hover(),
                );
                let p = ui.painter_at(rect);
                let cell_w = (width - LABEL_W) / HEAT_COLS as f32;
                let origin = pos2(rect.left() + LABEL_W, rect.top() + TOP);
                let cell_rect = |row: i64, col: i64| {
                    Rect::from_min_size(
                        origin + vec2(col as f32 * cell_w, row as f32 * ROW_H),
                        vec2(cell_w, ROW_H),
                    )
                };

                for hour in (0..=24).step_by(3) {
                    p.text(
                        pos2(origin.x + hour as f32 * 6.0 * cell_w, rect.top()),
                        if hour == 24 {
                            Align2::RIGHT_TOP
                        } else {
                            Align2::LEFT_TOP
                        },
                        format!("{hour:02} h"),
                        font(10.5),
                        theme::MUTED,
                    );
                }
                // El día más reciente arriba.
                for row in 0..HEAT_DAYS {
                    let day = HEAT_DAYS - 1 - row;
                    let ts = heat.day0 + day * 86_400;
                    p.text(
                        pos2(rect.left(), origin.y + (row as f32 + 0.5) * ROW_H),
                        Align2::LEFT_CENTER,
                        if row == 0 {
                            "Hoy".to_owned()
                        } else {
                            day_label(ts + 43_200)
                        },
                        font(11.0),
                        theme::MUTED,
                    );
                    for col in 0..HEAT_COLS {
                        let color = match &heat.cells[(day * HEAT_COLS + col) as usize] {
                            Some(point) => heat.color(point),
                            None => theme::GRID,
                        };
                        p.rect_filled(cell_rect(row, col).shrink(0.75), 1.5, color);
                    }
                }

                let hit = response.hover_pos().and_then(|pos| {
                    let col = ((pos.x - origin.x) / cell_w).floor() as i64;
                    let row = ((pos.y - origin.y) / ROW_H).floor() as i64;
                    ((0..HEAT_COLS).contains(&col) && (0..HEAT_DAYS).contains(&row))
                        .then_some((row, col))
                });
                if let Some((row, col)) = hit {
                    let day = HEAT_DAYS - 1 - row;
                    let ts = heat.day0 + day * 86_400 + col * HEAT_BUCKET;
                    p.rect_stroke(
                        cell_rect(row, col),
                        2.0,
                        Stroke::new(1.5, theme::INK),
                        StrokeKind::Outside,
                    );
                    let cell = heat.cells[(day * HEAT_COLS + col) as usize];
                    response.on_hover_ui_at_pointer(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{}  {} – {}",
                                day_label(ts),
                                format_ts(ts, "%H:%M"),
                                format_ts(ts + HEAT_BUCKET, "%H:%M")
                            ))
                            .color(theme::INK),
                        );
                        let Some(c) = cell else {
                            ui.label("Sin datos (monitor apagado o UPS desconectado)");
                            return;
                        };
                        if let (Some(avg), Some(lo), Some(hi)) = (c.vin_avg, c.vin_min, c.vin_max) {
                            ui.label(format!(
                                "Entrada: {} V de media ({} – {})",
                                num(avg, 1),
                                num(lo, 1),
                                num(hi, 1)
                            ));
                            ui.label(format!(
                                "{} % respecto de {} V",
                                signed(self.limits.deviation_pct(avg as f32), 1),
                                num(self.limits.nominal, 0)
                            ));
                        }
                        if c.n_batt > 0 {
                            ui.label(format!("En batería: {}", format_duration(c.n_batt)));
                        }
                        if c.n_avr > 0 {
                            ui.label(format!("AVR activo: {}", format_duration(c.n_avr)));
                        }
                    });
                }

                ui.horizontal(|ui| {
                    let caption = |ui: &mut Ui, text: String| {
                        ui.label(RichText::new(text).font(font(11.5)).color(theme::MUTED));
                    };
                    caption(ui, format!("{} V", num(heat.scale.0, 0)));
                    let (bar, _) = ui.allocate_exact_size(vec2(160.0, 12.0), Sense::hover());
                    const STEPS: usize = 32;
                    let step_w = bar.width() / STEPS as f32;
                    for i in 0..STEPS {
                        let seg = Rect::from_min_size(
                            bar.left_top() + vec2(i as f32 * step_w, 0.0),
                            vec2(step_w + 0.5, bar.height()),
                        );
                        ui.painter().rect_filled(
                            seg,
                            0.0,
                            theme::ramp(i as f32 / (STEPS - 1) as f32),
                        );
                    }
                    caption(ui, format!("{} V", num(heat.scale.1, 0)));
                    ui.add_space(12.0);
                    for (color, label) in [
                        (theme::CRITICAL, "Corte de luz"),
                        (theme::GRID, "Sin datos"),
                    ] {
                        let (swatch, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                        ui.painter().rect_filled(swatch, 2.0, color);
                        caption(ui, label.into());
                        ui.add_space(8.0);
                    }
                    caption(ui, "· cada celda son 10 minutos".into());
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_scale_has_a_minimum_span() {
        assert_eq!(heat_scale(&[], 220.0), (216.0, 224.0));
        assert_eq!(heat_scale(&[230.4, 232.1], 220.0), (228.0, 236.0));
        assert_eq!(heat_scale(&[205.2, 238.7], 220.0), (205.0, 239.0));
    }

    #[test]
    fn heat_scale_ignores_outliers() {
        let mut volts = vec![199.0];
        volts.extend((0..200).map(|i| 222.0 + f64::from(i) * 0.05));
        volts.push(251.0);
        assert_eq!(heat_scale(&volts, 220.0), (222.0, 232.0));
    }

    #[test]
    fn parses_options() {
        let parse = |args: &[&str]| {
            Options::parse(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&[]),
            Ok(Options {
                show: false,
                range: Range::H1,
                zoom: 1.0
            })
        );
        assert_eq!(
            parse(&["--show", "--range", "7d", "--zoom", "0.8"]),
            Ok(Options {
                show: true,
                range: Range::D7,
                zoom: 0.8
            })
        );
        assert!(parse(&["--range"]).is_err());
        assert!(parse(&["--range", "2h"]).is_err());
        assert!(parse(&["--zoom", "9"]).is_err());
        assert!(parse(&["--nope"]).is_err());
    }

    #[test]
    fn heat_color_marks_outages_first() {
        let heat = Heat {
            loaded: Instant::now(),
            day0: 0,
            cells: Vec::new(),
            scale: (220.0, 240.0),
        };
        let cell = |n_batt, vin_avg| Point {
            ts: 0,
            n_batt,
            n_avr: 0,
            vin_min: None,
            vin_max: None,
            vin_avg,
            freq_avg: None,
            vout_avg: 0.0,
            load_avg: 0.0,
            batt_min: 0.0,
        };
        assert_eq!(heat.color(&cell(0, Some(220.0))), theme::ramp(0.0));
        assert_eq!(heat.color(&cell(0, Some(230.0))), theme::ramp(0.5));
        // Un corte, por breve que sea, siempre se ve.
        assert_eq!(heat.color(&cell(1, Some(230.0))), theme::CRITICAL);
        assert_eq!(heat.color(&cell(600, None)), theme::CRITICAL);
    }

    #[test]
    fn percent_keeps_small_fractions_visible() {
        assert_eq!(percent(0, 0), "—");
        assert_eq!(percent(50, 100), "50,0 %");
        assert_eq!(percent(100, 100), "100,0 %");
        assert_eq!(percent(86_399, 86_400), "99,999 %");
        assert_eq!(percent(1, 86_400), "0,001 %");
    }

    #[test]
    fn signed_uses_typographic_minus() {
        assert_eq!(signed(5.04, 1), "+5,0");
        assert_eq!(signed(-3.26, 1), "−3,3");
    }
}
