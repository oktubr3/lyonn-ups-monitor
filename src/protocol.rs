//! Protocolo Megatec/Voltronic (`QS`, `F`) tal como lo habla el Lyonn CTB-800V.

pub const VENDOR_ID: u16 = 0x0665;
pub const PRODUCT_ID: u16 = 0x5161;

/// Bits de estado de la respuesta `QS`, en el orden en que llegan (b7..b0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(pub u8);

impl Flags {
    pub const ON_BATTERY: u8 = 1 << 7;
    pub const BATT_LOW: u8 = 1 << 6;
    pub const AVR_ACTIVE: u8 = 1 << 5;
    pub const UPS_FAILED: u8 = 1 << 4;
    // El bit 3 indica el tipo de UPS (1 = line-interactive): es constante,
    // no un evento, así que no se expone.
    pub const TEST_MODE: u8 = 1 << 2;
    pub const SHUTDOWN_ACTIVE: u8 = 1 << 1;
    pub const BEEPER_ON: u8 = 1;

    fn parse(s: &str) -> Option<Self> {
        if s.len() != 8 || !s.bytes().all(|b| b == b'0' || b == b'1') {
            return None;
        }
        u8::from_str_radix(s, 2).ok().map(Self)
    }

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    pub fn on_battery(self) -> bool {
        self.has(Self::ON_BATTERY)
    }
    pub fn batt_low(self) -> bool {
        self.has(Self::BATT_LOW)
    }
    pub fn avr_active(self) -> bool {
        self.has(Self::AVR_ACTIVE)
    }
    pub fn ups_failed(self) -> bool {
        self.has(Self::UPS_FAILED)
    }
}

/// Respuesta a `QS`: `(VIN VFAULT VOUT LOAD FREQ VBATT TEMP FLAGS`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    pub input_v: f32,
    /// Tensión de entrada registrada en la última transferencia a batería/AVR.
    pub fault_v: f32,
    pub output_v: f32,
    pub load_pct: u8,
    pub freq_hz: f32,
    pub batt_v: f32,
    /// El CTB-800V no tiene sensor y responde `--.-`.
    pub temp_c: Option<f32>,
    pub flags: Flags,
}

/// Respuesta a `F`: `#VNOM INOM VBATT FNOM`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rating {
    pub voltage: f32,
    pub current_a: f32,
    pub batt_v: f32,
    pub freq_hz: f32,
}

impl Default for Rating {
    fn default() -> Self {
        Self {
            voltage: 220.0,
            current_a: 2.0,
            batt_v: 12.0,
            freq_hz: 50.0,
        }
    }
}

/// Reporte HID de salida: report ID 0 + comando rellenado a 8 bytes.
pub fn frame(cmd: &str) -> [u8; 9] {
    let mut out = [0u8; 9];
    let bytes = cmd.as_bytes();
    let n = bytes.len().min(7);
    out[1..=n].copy_from_slice(&bytes[..n]);
    out[n + 1] = b'\r';
    out
}

pub fn parse_status(line: &str) -> Option<Status> {
    let mut parts = line.trim().strip_prefix('(')?.split_ascii_whitespace();
    let status = Status {
        input_v: parts.next()?.parse().ok()?,
        fault_v: parts.next()?.parse().ok()?,
        output_v: parts.next()?.parse().ok()?,
        load_pct: parts.next()?.parse().ok()?,
        freq_hz: parts.next()?.parse().ok()?,
        batt_v: parts.next()?.parse().ok()?,
        temp_c: parts.next()?.parse().ok(),
        flags: Flags::parse(parts.next()?)?,
    };
    Some(status)
}

pub fn parse_rating(line: &str) -> Option<Rating> {
    let mut parts = line.trim().strip_prefix('#')?.split_ascii_whitespace();
    Some(Rating {
        voltage: parts.next()?.parse().ok()?,
        current_a: parts.next()?.parse().ok()?,
        batt_v: parts.next()?.parse().ok()?,
        freq_hz: parts.next()?.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Respuestas capturadas del equipo real.
    const QS: &str = "(230.2 232.2 234.6 000 49.8 13.6 --.- 00001001";
    const F: &str = "#220.0 002 12.00 50.0";

    #[test]
    fn parses_captured_status() {
        let s = parse_status(QS).unwrap();
        assert_eq!(s.input_v, 230.2);
        assert_eq!(s.fault_v, 232.2);
        assert_eq!(s.output_v, 234.6);
        assert_eq!(s.load_pct, 0);
        assert_eq!(s.freq_hz, 49.8);
        assert_eq!(s.batt_v, 13.6);
        assert_eq!(s.temp_c, None);
        assert!(!s.flags.on_battery());
        assert!(!s.flags.avr_active());
        assert_eq!(s.flags.0, 0b0000_1001);
        assert!(s.flags.has(Flags::BEEPER_ON));
    }

    #[test]
    fn parses_outage_flags() {
        let s = parse_status("(000.0 198.0 220.0 035 50.0 12.1 25.0 11001001").unwrap();
        assert!(s.flags.on_battery());
        assert!(s.flags.batt_low());
        assert_eq!(s.temp_c, Some(25.0));
        assert_eq!(s.load_pct, 35);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_status(""), None);
        assert_eq!(parse_status("I"), None);
        assert_eq!(parse_status("(230.2 232.2"), None);
        assert_eq!(
            parse_status("(230.2 232.2 234.6 000 49.8 13.6 --.- 0000100"),
            None
        );
        assert_eq!(
            parse_status("(230.2 232.2 234.6 000 49.8 13.6 --.- 0000200x"),
            None
        );
        assert_eq!(
            parse_status("(abc 232.2 234.6 000 49.8 13.6 --.- 00001001"),
            None
        );
    }

    #[test]
    fn parses_captured_rating() {
        let r = parse_rating(F).unwrap();
        assert_eq!(r.voltage, 220.0);
        assert_eq!(r.current_a, 2.0);
        assert_eq!(r.batt_v, 12.0);
        assert_eq!(r.freq_hz, 50.0);
    }

    #[test]
    fn frames_are_zero_padded_reports() {
        assert_eq!(frame("QS"), [0, b'Q', b'S', b'\r', 0, 0, 0, 0, 0]);
        assert_eq!(frame("F"), [0, b'F', b'\r', 0, 0, 0, 0, 0, 0]);
    }
}
