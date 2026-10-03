//! Subcomandos de terminal.

use std::io::Write;
use std::time::Duration;

use chrono::{Local, TimeZone};

use crate::device::Ups;
use crate::model::{
    GridState, Limits, Severity, battery_pct, format_duration, load_watts, runtime_minutes,
};
use crate::monitor::unix_now;
use crate::protocol::{Flags, Rating, Status};
use crate::store::Store;

const RED: &str = "\x1b[0;31m";
const YELLOW: &str = "\x1b[1;33m";
const GREEN: &str = "\x1b[0;32m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const NC: &str = "\x1b[0m";

/// Antigüedad máxima de una muestra del historial para darla por actual.
const FRESH_SECS: i64 = 5;

pub const USAGE: &str = "\
ups-monitor — monitor de red eléctrica y UPS (Lyonn CTB-800V)

  ups-monitor            App de barra de menú (el tablero se abre desde el menú)
      --show             Abre el tablero al iniciar
      --range PERÍODO    Período inicial: 15m, 1h, 6h, 24h, 7d o 30d
      --zoom FACTOR      Tamaño de la interfaz (1 = normal)
  ups-monitor status     Estado actual
  ups-monitor watch      Estado en tiempo real (Ctrl+C para salir)
  ups-monitor events [N] Últimos N eventos registrados (por defecto 20)
  ups-monitor raw        Respuestas crudas del UPS (QS y F)
";

/// De dónde sale una lectura: del UPS o del historial que escribe la app.
enum Source {
    Device(Ups, Rating),
    Daemon(Store),
}

impl Source {
    /// Si la app de barra de menú está corriendo tiene el dispositivo tomado,
    /// así que se lee lo último que guardó en vez de competir por el USB.
    fn open() -> Result<Self, String> {
        if let Ok(store) = Store::open()
            && let Ok(Some((ts, _))) = store.latest()
            && unix_now() - ts <= FRESH_SECS
        {
            return Ok(Source::Daemon(store));
        }
        let ups =
            Ups::open().map_err(|e| format!("UPS no encontrado ({e}). Verificá el cable USB."))?;
        let rating = ups.rating().unwrap_or_default();
        Ok(Source::Device(ups, rating))
    }

    fn read(&self) -> Result<(Status, Rating), String> {
        match self {
            Source::Device(ups, rating) => Ok((ups.status()?, *rating)),
            Source::Daemon(store) => match store.latest().map_err(|e| e.to_string())? {
                Some((ts, status)) if unix_now() - ts <= FRESH_SECS => {
                    Ok((status, Rating::default()))
                }
                _ => Err("la app dejó de registrar lecturas".into()),
            },
        }
    }
}

fn severity_color(severity: Severity) -> &'static str {
    match severity {
        Severity::Good => GREEN,
        Severity::Warning => YELLOW,
        Severity::Serious | Severity::Critical => RED,
        Severity::Unknown => DIM,
    }
}

fn render(s: &Status, rating: &Rating) -> String {
    let limits = Limits::new(rating.voltage);
    let state = GridState::of(s, &limits);
    let pct = battery_pct(s, rating);
    let filled = usize::from(pct) / 5;
    let bar_color = match pct {
        60.. => GREEN,
        30.. => YELLOW,
        _ => RED,
    };
    let runtime =
        runtime_minutes(s, rating).map_or("— (sin carga medida)".into(), |m| format!("~{m} min"));
    let flags: Vec<&str> = [
        (Flags::AVR_ACTIVE, "AVR"),
        (Flags::TEST_MODE, "TEST"),
        (Flags::SHUTDOWN_ACTIVE, "SHUTDOWN"),
        (Flags::BEEPER_ON, "BEEPER"),
    ]
    .into_iter()
    .filter(|&(bit, _)| s.flags.has(bit))
    .map(|(_, name)| name)
    .collect();

    let mut out = String::new();
    let mut line = |l: String| {
        out.push_str(&l);
        out.push('\n');
    };
    line(format!("{BOLD}  UPS — Lyonn CTB-800V{NC}"));
    line(String::new());
    line(format!(
        "  Estado            {}● {}{NC}",
        severity_color(state.severity()),
        state.title()
    ));
    line(String::new());
    if s.flags.on_battery() {
        line("  Tensión entrada   —".into());
    } else {
        line(format!(
            "  Tensión entrada   {:.1} V  ({:+.1} % de {:.0} V)",
            s.input_v,
            limits.deviation_pct(s.input_v),
            limits.nominal
        ));
    }
    line(format!("  Tensión salida    {:.1} V", s.output_v));
    line(format!("  Frecuencia        {:.1} Hz", s.freq_hz));
    line(format!(
        "  Carga             {} %  (~{:.0} W)",
        s.load_pct,
        load_watts(s.load_pct)
    ));
    line(String::new());
    line(format!(
        "  Batería           [{bar_color}{}{NC}{}] {pct} %  ({:.1} V)",
        "█".repeat(filled),
        "░".repeat(20 - filled),
        s.batt_v
    ));
    line(format!("  Autonomía est.    {runtime}"));
    if !flags.is_empty() {
        line(format!("  Flags             {}", flags.join(", ")));
    }
    out
}

pub fn status() -> Result<(), String> {
    let (s, rating) = Source::open()?.read()?;
    print!("{}", render(&s, &rating));
    Ok(())
}

pub fn watch() -> Result<(), String> {
    let source = Source::open()?;
    loop {
        let body = match source.read() {
            Ok((s, rating)) => render(&s, &rating),
            Err(e) => format!("  {RED}{e}{NC}\n"),
        };
        print!(
            "\x1b[2J\x1b[H{body}\n  {DIM}{} — Ctrl+C para salir{NC}\n",
            Local::now().format("%H:%M:%S")
        );
        std::io::stdout().flush().ok();
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub fn raw() -> Result<(), String> {
    let ups = Ups::open().map_err(|e| {
        format!("no se pudo abrir el UPS ({e}). Si la app de barra de menú está corriendo, cerrala primero.")
    })?;
    for cmd in ["QS", "F"] {
        match ups.query(cmd) {
            Ok(line) => println!("{cmd}: {line}"),
            Err(e) => println!("{cmd}: error: {e}"),
        }
    }
    Ok(())
}

pub fn events(limit: i64) -> Result<(), String> {
    let store = Store::open().map_err(|e| e.to_string())?;
    let events = store
        .events(0, i64::MAX, limit)
        .map_err(|e| e.to_string())?;
    if events.is_empty() {
        println!("Sin eventos registrados.");
        return Ok(());
    }
    let now = unix_now();
    for e in events {
        let start = Local
            .timestamp_opt(e.ts_start, 0)
            .single()
            .map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M:%S").to_string());
        let duration = match e.ts_end {
            Some(end) => format_duration(end - e.ts_start),
            None => format!("{} (en curso)", format_duration(now - e.ts_start)),
        };
        let value = e.value.map_or_else(String::new, |v| {
            format!("  {} {v:.1} V", e.kind.value_label())
        });
        println!("{start}  {:<14} {duration:<18}{value}", e.kind.label());
    }
    Ok(())
}
