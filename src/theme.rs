//! Paleta y tipografía del tablero (tema oscuro).

use eframe::egui::{Color32, FontId};

use crate::model::{Band, Severity};

pub const PLANE: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0d);
pub const SURFACE: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x19);
pub const INK: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
pub const INK_2: Color32 = Color32::from_rgb(0xc3, 0xc2, 0xb7);
pub const MUTED: Color32 = Color32::from_rgb(0x89, 0x87, 0x81);
pub const GRID: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2a);
pub const AXIS: Color32 = Color32::from_rgb(0x38, 0x38, 0x35);

// Una magnitud, un color: se mantiene en todos los gráficos.
pub const VIN: Color32 = Color32::from_rgb(0x39, 0x87, 0xe5);
pub const VOUT: Color32 = Color32::from_rgb(0xd9, 0x59, 0x26);
pub const FREQ: Color32 = Color32::from_rgb(0x19, 0x9e, 0x70);
pub const LOAD: Color32 = Color32::from_rgb(0xc9, 0x85, 0x00);
pub const BATT: Color32 = Color32::from_rgb(0xd5, 0x51, 0x81);

// Colores de estado: reservados, nunca se usan para una serie.
pub const GOOD: Color32 = Color32::from_rgb(0x0c, 0xa3, 0x0c);
pub const WARNING: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x19);
pub const SERIOUS: Color32 = Color32::from_rgb(0xec, 0x83, 0x5a);
pub const CRITICAL: Color32 = Color32::from_rgb(0xd0, 0x3b, 0x3b);

pub fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

pub fn border() -> Color32 {
    alpha(INK, 26)
}

pub fn severity(s: Severity) -> Color32 {
    match s {
        Severity::Good => GOOD,
        Severity::Warning => WARNING,
        Severity::Serious => SERIOUS,
        Severity::Critical => CRITICAL,
        Severity::Unknown => MUTED,
    }
}

pub fn band(b: Band) -> Color32 {
    match b {
        Band::Good => GOOD,
        Band::Marginal => WARNING,
        Band::Out => SERIOUS,
        Band::Outage => CRITICAL,
    }
}

/// Rampa secuencial de un solo tono para magnitudes: `t` de 0 (bajo) a 1 (alto).
pub fn ramp(t: f32) -> Color32 {
    const STOPS: [[u8; 3]; 5] = [
        [0x0d, 0x36, 0x6b],
        [0x1c, 0x5c, 0xab],
        [0x39, 0x87, 0xe5],
        [0x86, 0xb6, 0xef],
        [0xcd, 0xe2, 0xfb],
    ];
    let x = t.clamp(0.0, 1.0) * (STOPS.len() - 1) as f32;
    let i = (x as usize).min(STOPS.len() - 2);
    let f = x - i as f32;
    let mix = |c: usize| {
        (f32::from(STOPS[i][c]) * (1.0 - f) + f32::from(STOPS[i + 1][c]) * f).round() as u8
    };
    Color32::from_rgb(mix(0), mix(1), mix(2))
}

pub fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_interpolates_and_clamps() {
        assert_eq!(ramp(-1.0), Color32::from_rgb(0x0d, 0x36, 0x6b));
        assert_eq!(ramp(0.5), Color32::from_rgb(0x39, 0x87, 0xe5));
        assert_eq!(ramp(2.0), Color32::from_rgb(0xcd, 0xe2, 0xfb));
        // A mitad de camino entre los dos primeros tonos.
        assert_eq!(ramp(0.125), Color32::from_rgb(0x15, 0x49, 0x8b));
    }
}
