//! Reglas de dominio: calidad de la red, batería y autonomía.

use crate::protocol::{Rating, Status};

/// Desvío respecto de la nominal dentro del cual la tensión es óptima.
pub const TOL_GOOD: f32 = 0.05;
/// Tolerancia reglamentaria de la distribuidora (ENRE, ±8 %).
pub const TOL_NORM: f32 = 0.08;
/// Tolerancia de frecuencia para considerarla estable.
pub const TOL_FREQ_HZ: f32 = 0.5;

// El CTB-800V es un equipo de 800 VA / 480 W con una batería de 12 V 7 Ah.
const UPS_WATTS: f32 = 480.0;
const BATTERY_WH: f32 = 84.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Good,
    Marginal,
    Out,
    Outage,
}

impl Band {
    pub fn label(self) -> &'static str {
        match self {
            Band::Good => "Óptima (±5 %)",
            Band::Marginal => "Tolerable (±8 %)",
            Band::Out => "Fuera de norma",
            Band::Outage => "Corte",
        }
    }
}

/// Límites absolutos de tensión para una nominal dada.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub nominal: f32,
    pub good_lo: f32,
    pub good_hi: f32,
    pub norm_lo: f32,
    pub norm_hi: f32,
}

impl Limits {
    pub fn new(nominal: f32) -> Self {
        Self {
            nominal,
            good_lo: nominal * (1.0 - TOL_GOOD),
            good_hi: nominal * (1.0 + TOL_GOOD),
            norm_lo: nominal * (1.0 - TOL_NORM),
            norm_hi: nominal * (1.0 + TOL_NORM),
        }
    }

    pub fn band(&self, input_v: f32) -> Band {
        if input_v >= self.good_lo && input_v <= self.good_hi {
            Band::Good
        } else if input_v >= self.norm_lo && input_v <= self.norm_hi {
            Band::Marginal
        } else {
            Band::Out
        }
    }

    /// Límites en el orden que esperan las consultas: óptima y luego norma.
    pub fn sql_params(&self) -> [f64; 4] {
        [self.good_lo, self.good_hi, self.norm_lo, self.norm_hi].map(f64::from)
    }

    pub fn deviation_pct(&self, input_v: f32) -> f32 {
        (input_v / self.nominal - 1.0) * 100.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Good,
    Warning,
    Serious,
    Critical,
    Unknown,
}

/// Estado global, de más grave a menos grave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridState {
    Disconnected,
    Failed,
    Outage,
    OutOfNorm,
    Avr,
    Normal,
}

impl GridState {
    pub fn of(status: &Status, limits: &Limits) -> Self {
        let f = status.flags;
        if f.ups_failed() {
            GridState::Failed
        } else if f.on_battery() {
            GridState::Outage
        } else {
            match limits.band(status.input_v) {
                Band::Out | Band::Outage => GridState::OutOfNorm,
                _ if f.avr_active() => GridState::Avr,
                // Dentro de la tolerancia reglamentaria la red está normal; el
                // matiz óptima/tolerable queda para los gráficos.
                Band::Good | Band::Marginal => GridState::Normal,
            }
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            GridState::Disconnected => "UPS no detectado",
            GridState::Failed => "Falla en el UPS",
            GridState::Outage => "Corte de luz — en batería",
            GridState::OutOfNorm => "Tensión fuera de norma",
            GridState::Avr => "Regulador (AVR) activo",
            GridState::Normal => "Red eléctrica normal",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            GridState::Disconnected => "?",
            GridState::Failed => "❌",
            GridState::Outage => "🔋",
            GridState::OutOfNorm | GridState::Avr => "⚠️",
            GridState::Normal => "⚡",
        }
    }

    pub fn severity(self) -> Severity {
        match self {
            GridState::Disconnected => Severity::Unknown,
            GridState::Failed | GridState::Outage => Severity::Critical,
            GridState::OutOfNorm => Severity::Serious,
            GridState::Avr => Severity::Warning,
            GridState::Normal => Severity::Good,
        }
    }
}

/// Carga estimada de la batería a partir de su tensión.
///
/// En red la batería está en flote (≈13,6 V), así que la tensión solo indica si
/// terminó de cargar; en descarga se usa la curva de una plomo-ácido bajo carga.
pub fn battery_pct(status: &Status, rating: &Rating) -> u8 {
    const CHARGING: [(f32, f32); 4] = [(12.0, 20.0), (12.6, 60.0), (13.0, 85.0), (13.4, 100.0)];
    const DISCHARGING: [(f32, f32); 6] = [
        (10.5, 0.0),
        (11.2, 10.0),
        (11.6, 25.0),
        (12.0, 50.0),
        (12.4, 80.0),
        (12.7, 100.0),
    ];
    let v = status.batt_v * 12.0 / rating.batt_v.max(1.0);
    let curve: &[(f32, f32)] = if status.flags.on_battery() {
        &DISCHARGING
    } else {
        &CHARGING
    };
    interpolate(curve, v).round() as u8
}

fn interpolate(curve: &[(f32, f32)], x: f32) -> f32 {
    let (first, last) = (curve[0], curve[curve.len() - 1]);
    if x <= first.0 {
        return first.1;
    }
    if x >= last.0 {
        return last.1;
    }
    for pair in curve.windows(2) {
        let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
        if x <= x1 {
            return y0 + (y1 - y0) * (x - x0) / (x1 - x0);
        }
    }
    last.1
}

pub fn load_watts(load_pct: u8) -> f32 {
    UPS_WATTS * f32::from(load_pct) / 100.0
}

/// Autonomía estimada en minutos; `None` si el UPS no mide carga.
pub fn runtime_minutes(status: &Status, rating: &Rating) -> Option<u32> {
    if status.load_pct == 0 {
        return None;
    }
    let wh = BATTERY_WH * f32::from(battery_pct(status, rating)) / 100.0;
    Some((wh / load_watts(status.load_pct) * 60.0) as u32)
}

/// Número con coma decimal, como se escribe en español.
pub fn num(v: impl Into<f64>, decimals: usize) -> String {
    format!("{:.decimals$}", v.into()).replace('.', ",")
}

pub fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (d, h, m, s) = (
        secs / 86400,
        secs % 86400 / 3600,
        secs % 3600 / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d} d {h} h")
    } else if h > 0 {
        format!("{h} h {m:02} min")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_status;

    fn status(line: &str) -> Status {
        parse_status(line).unwrap()
    }

    #[test]
    fn bands_follow_nominal() {
        let l = Limits::new(220.0);
        assert_eq!(l.band(220.0), Band::Good);
        assert_eq!(l.band(230.9), Band::Good);
        assert_eq!(l.band(232.0), Band::Marginal);
        assert_eq!(l.band(205.0), Band::Marginal);
        assert_eq!(l.band(238.0), Band::Out);
        assert_eq!(l.band(190.0), Band::Out);
        assert!((l.deviation_pct(231.0) - 5.0).abs() < 0.01);
    }

    #[test]
    fn grid_state_priority() {
        let l = Limits::new(220.0);
        let s = |line| GridState::of(&status(line), &l);
        assert_eq!(
            s("(221.0 221.0 221.0 010 50.0 13.6 --.- 00001001"),
            GridState::Normal
        );
        assert_eq!(
            s("(233.0 221.0 221.0 010 50.0 13.6 --.- 00001001"),
            GridState::Normal
        );
        assert_eq!(
            s("(233.0 221.0 221.0 010 50.0 13.6 --.- 00101001"),
            GridState::Avr
        );
        assert_eq!(
            s("(245.0 221.0 221.0 010 50.0 13.6 --.- 00101001"),
            GridState::OutOfNorm
        );
        assert_eq!(
            s("(000.0 221.0 221.0 010 50.0 12.4 --.- 10001001"),
            GridState::Outage
        );
        assert_eq!(
            s("(000.0 221.0 221.0 010 50.0 12.4 --.- 10011001"),
            GridState::Failed
        );
    }

    #[test]
    fn battery_estimate() {
        let r = Rating::default();
        let float = status("(221.0 221.0 221.0 010 50.0 13.6 --.- 00001001");
        assert_eq!(battery_pct(&float, &r), 100);
        let discharging = status("(000.0 221.0 221.0 010 50.0 12.0 --.- 10001001");
        assert_eq!(battery_pct(&discharging, &r), 50);
        let empty = status("(000.0 221.0 221.0 010 50.0 10.0 --.- 11001001");
        assert_eq!(battery_pct(&empty, &r), 0);
    }

    #[test]
    fn runtime_needs_load() {
        let r = Rating::default();
        let idle = status("(221.0 221.0 221.0 000 50.0 13.6 --.- 00001001");
        assert_eq!(runtime_minutes(&idle, &r), None);
        // 84 Wh al 100 % con 25 % de 480 W = 120 W → 42 min.
        let loaded = status("(221.0 221.0 221.0 025 50.0 13.6 --.- 00001001");
        assert_eq!(runtime_minutes(&loaded, &r), Some(42));
    }

    #[test]
    fn numbers_use_decimal_comma() {
        assert_eq!(num(231.25_f32, 1), "231,2");
        assert_eq!(num(50.0, 0), "50");
        assert_eq!(num(-0.5, 2), "-0,50");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(42), "42 s");
        assert_eq!(format_duration(125), "2 min 05 s");
        assert_eq!(format_duration(3_720), "1 h 02 min");
        assert_eq!(format_duration(90_000), "1 d 1 h");
    }
}
