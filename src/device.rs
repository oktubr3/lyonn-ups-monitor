//! Acceso USB HID al UPS.

use std::time::Duration;

use hidapi::{HidApi, HidDevice};

use crate::protocol::{self, Rating, Status};

const READ_TIMEOUT_MS: i32 = 500;
const MAX_CHUNKS: usize = 12;

pub struct Ups {
    dev: HidDevice,
}

impl Ups {
    pub fn open() -> Result<Self, String> {
        // Se abre por VID/PID: enumerar todo el bus en cada intento no aporta nada.
        HidApi::disable_device_discovery();
        let api = HidApi::new().map_err(|e| e.to_string())?;
        let dev = api
            .open(protocol::VENDOR_ID, protocol::PRODUCT_ID)
            .map_err(|e| e.to_string())?;
        Ok(Self { dev })
    }

    /// Envía un comando y devuelve la línea de respuesta, sin el `\r` final.
    pub fn query(&self, cmd: &str) -> Result<String, String> {
        // Descarta restos de una respuesta anterior leída a medias.
        let mut chunk = [0u8; 8];
        while self
            .dev
            .read_timeout(&mut chunk, 0)
            .map_err(|e| e.to_string())?
            > 0
        {}

        self.dev
            .write(&protocol::frame(cmd))
            .map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(150));

        let mut buf = Vec::with_capacity(64);
        for _ in 0..MAX_CHUNKS {
            let n = self
                .dev
                .read_timeout(&mut chunk, READ_TIMEOUT_MS)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.iter().position(|&b| b == b'\r') {
                buf.truncate(end);
                return Ok(String::from_utf8_lossy(&buf).into_owned());
            }
        }
        Err(format!(
            "respuesta incompleta a {cmd}: {:?}",
            String::from_utf8_lossy(&buf)
        ))
    }

    pub fn status(&self) -> Result<Status, String> {
        let line = self.query("QS")?;
        protocol::parse_status(&line).ok_or_else(|| format!("respuesta QS inválida: {line:?}"))
    }

    pub fn rating(&self) -> Result<Rating, String> {
        let line = self.query("F")?;
        protocol::parse_rating(&line).ok_or_else(|| format!("respuesta F inválida: {line:?}"))
    }
}
