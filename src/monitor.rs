//! Hilo de sondeo: lee el UPS una vez por segundo, guarda el historial,
//! detecta eventos y publica el último estado. Nunca toca la interfaz.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::device::Ups;
use crate::model::{Limits, format_duration};
use crate::notify::{log, notify};
use crate::protocol::{Rating, Status};
use crate::store::{EventKind, Store};

pub const POLL: Duration = Duration::from_secs(1);
const RECONNECT: Duration = Duration::from_secs(3);
/// Lecturas fallidas seguidas antes de dar el dispositivo por perdido.
const MAX_ERRORS: u32 = 3;

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Último estado conocido, compartido con la interfaz.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub connected: bool,
    pub status: Option<Status>,
    /// Momento de la última lectura válida.
    pub ts: i64,
    pub rating: Rating,
    pub error: Option<String>,
    /// Se incrementa con cada novedad.
    pub generation: u64,
}

pub type Shared = Arc<Mutex<Live>>;

/// Sigue un tipo de evento con histéresis para no abrir y cerrar episodios
/// cuando una medición oscila sobre el umbral.
struct Tracker {
    kind: EventKind,
    enter_after: u32,
    exit_after: u32,
    streak: u32,
    streak_start: i64,
    open: Option<OpenEvent>,
}

struct OpenEvent {
    id: Option<i64>,
    ts_start: i64,
    value: f64,
}

enum Transition {
    Started,
    Ended { duration: i64 },
}

impl Tracker {
    fn new(kind: EventKind, enter_after: u32, exit_after: u32) -> Self {
        Self {
            kind,
            enter_after,
            exit_after,
            streak: 0,
            streak_start: 0,
            open: None,
        }
    }

    /// `value` es la medición asociada; `keep` elige cuál conservar (mín/máx).
    fn step(
        &mut self,
        store: Option<&Store>,
        ts: i64,
        active: bool,
        value: f64,
        keep: fn(f64, f64) -> f64,
    ) -> Option<Transition> {
        match (&mut self.open, active) {
            (None, true) => {
                if self.streak == 0 {
                    self.streak_start = ts;
                }
                self.streak += 1;
                if self.streak < self.enter_after {
                    return None;
                }
                self.streak = 0;
                let ts_start = self.streak_start;
                let id = store.and_then(|s| s.open_event(self.kind, ts_start, Some(value)).ok());
                self.open = Some(OpenEvent {
                    id,
                    ts_start,
                    value,
                });
                Some(Transition::Started)
            }
            (None, false) => {
                self.streak = 0;
                None
            }
            (Some(ev), true) => {
                self.streak = 0;
                let kept = keep(ev.value, value);
                if kept != ev.value {
                    ev.value = kept;
                    if let (Some(store), Some(id)) = (store, ev.id) {
                        store.update_event(id, Some(kept)).ok();
                    }
                }
                None
            }
            (Some(_), false) => {
                if self.streak == 0 {
                    self.streak_start = ts;
                }
                self.streak += 1;
                if self.streak < self.exit_after {
                    return None;
                }
                let end = self.streak_start;
                self.streak = 0;
                self.close(store, end)
            }
        }
    }

    fn close(&mut self, store: Option<&Store>, ts: i64) -> Option<Transition> {
        let ev = self.open.take()?;
        if let (Some(store), Some(id)) = (store, ev.id) {
            store.close_event(id, ts).ok();
        }
        Some(Transition::Ended {
            duration: ts - ev.ts_start,
        })
    }
}

fn first(a: f64, _: f64) -> f64 {
    a
}

struct Events {
    outage: Tracker,
    avr: Tracker,
    sag: Tracker,
    swell: Tracker,
    batt_low: Tracker,
    failure: Tracker,
}

impl Events {
    fn new() -> Self {
        Self {
            outage: Tracker::new(EventKind::Outage, 1, 1),
            avr: Tracker::new(EventKind::Avr, 1, 2),
            sag: Tracker::new(EventKind::Sag, 2, 5),
            swell: Tracker::new(EventKind::Swell, 2, 5),
            batt_low: Tracker::new(EventKind::BattLow, 1, 2),
            failure: Tracker::new(EventKind::Failure, 1, 2),
        }
    }

    fn step(&mut self, store: Option<&Store>, ts: i64, s: &Status, limits: &Limits) {
        let f = s.flags;
        let (vin, batt) = (f64::from(s.input_v), f64::from(s.batt_v));
        let on_grid = !f.on_battery();

        match self.outage.step(store, ts, f.on_battery(), batt, f64::min) {
            Some(Transition::Started) => {
                log("ALERTA: corte de luz, UPS en batería");
                notify(
                    "UPS — Se fue la luz",
                    "El UPS pasó a batería. Guardá tu trabajo.",
                );
            }
            Some(Transition::Ended { duration }) => {
                let d = format_duration(duration);
                log(&format!("OK: volvió la luz (corte de {d})"));
                notify("UPS — Volvió la luz", &format!("El corte duró {d}."));
            }
            None => {}
        }

        if let Some(Transition::Started) =
            self.batt_low.step(store, ts, f.batt_low(), batt, f64::min)
        {
            log("CRITICO: batería baja");
            notify(
                "UPS — Batería crítica",
                "Batería muy baja. Apagá el equipo ahora.",
            );
        }

        if let Some(Transition::Started) = self.failure.step(store, ts, f.ups_failed(), vin, first)
        {
            log("CRITICO: el UPS reporta una falla");
            notify("UPS — Falla", "El UPS reporta una falla interna.");
        }

        match self
            .avr
            .step(store, ts, on_grid && f.avr_active(), vin, first)
        {
            Some(Transition::Started) => log(&format!("AVR activo (entrada {vin:.1} V)")),
            Some(Transition::Ended { duration }) => {
                log(&format!(
                    "AVR inactivo (duró {})",
                    format_duration(duration)
                ));
            }
            None => {}
        }

        let low = on_grid && s.input_v < limits.norm_lo;
        if let Some(Transition::Started) = self.sag.step(store, ts, low, vin, f64::min) {
            log(&format!("Baja tensión: {vin:.1} V"));
            notify("UPS — Baja tensión", &format!("La red está en {vin:.0} V."));
        }

        let high = on_grid && s.input_v > limits.norm_hi;
        if let Some(Transition::Started) = self.swell.step(store, ts, high, vin, f64::max) {
            log(&format!("Sobretensión: {vin:.1} V"));
            notify("UPS — Sobretensión", &format!("La red está en {vin:.0} V."));
        }
    }

    /// Cierra todo lo abierto cuando se pierde el dispositivo.
    fn close_all(&mut self, store: Option<&Store>, ts: i64) {
        for t in [
            &mut self.outage,
            &mut self.avr,
            &mut self.sag,
            &mut self.swell,
            &mut self.batt_low,
            &mut self.failure,
        ] {
            t.streak = 0;
            t.close(store, ts);
        }
    }
}

/// Lanza el hilo de sondeo. `wake` se llama tras cada novedad y debe ser
/// seguro desde cualquier hilo (p. ej. pedir un repintado).
pub fn spawn(shared: Shared, wake: impl Fn() + Send + 'static) {
    std::thread::Builder::new()
        .name("ups-poll".into())
        .spawn(move || run(&shared, &wake))
        .expect("no se pudo crear el hilo de sondeo");
}

fn publish(shared: &Shared, wake: &dyn Fn(), update: impl FnOnce(&mut Live)) {
    if let Ok(mut live) = shared.lock() {
        update(&mut live);
        live.generation += 1;
    }
    wake();
}

fn run(shared: &Shared, wake: &dyn Fn()) {
    let store = match Store::open() {
        Ok(store) => Some(store),
        Err(e) => {
            log(&format!("ERROR: no se pudo abrir el historial: {e}"));
            None
        }
    };
    let store = store.as_ref();
    if let Some(store) = store {
        store.close_stale_events().ok();
    }

    let mut limits = Limits::new(Rating::default().voltage);
    let mut events = Events::new();
    let mut rolled_minute = 0;
    let mut last_ts = 0;

    loop {
        let ups = match Ups::open() {
            Ok(ups) => ups,
            Err(e) => {
                publish(shared, wake, |live| {
                    live.connected = false;
                    live.error = Some(e);
                });
                std::thread::sleep(RECONNECT);
                continue;
            }
        };
        if let Ok(rating) = ups.rating() {
            limits = Limits::new(rating.voltage);
            if let Ok(mut live) = shared.lock() {
                live.rating = rating;
            }
        }

        let mut errors = 0;
        let mut next = Instant::now();
        while errors < MAX_ERRORS {
            match ups.status() {
                Ok(status) => {
                    errors = 0;
                    let ts = unix_now();
                    last_ts = ts;
                    if let Some(store) = store {
                        store.insert_sample(ts, &status).ok();
                        if ts / 60 != rolled_minute {
                            rolled_minute = ts / 60;
                            store.rollup(ts, &limits).ok();
                        }
                    }
                    events.step(store, ts, &status, &limits);
                    publish(shared, wake, |live| {
                        live.connected = true;
                        live.status = Some(status);
                        live.ts = ts;
                        live.error = None;
                    });
                }
                Err(e) => {
                    errors += 1;
                    if let Ok(mut live) = shared.lock() {
                        live.error = Some(e);
                    }
                }
            }
            next += POLL;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                // Tras una suspensión del equipo no se recuperan los ciclos perdidos.
                next = now;
            }
        }

        events.close_all(store, last_ts);
        publish(shared, wake, |live| live.connected = false);
        std::thread::sleep(RECONNECT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_debounces_both_edges() {
        let mut t = Tracker::new(EventKind::Sag, 2, 3);
        // Un pico aislado no abre el evento.
        assert!(t.step(None, 10, true, 200.0, f64::min).is_none());
        assert!(t.step(None, 11, false, 220.0, f64::min).is_none());
        assert!(t.open.is_none());

        assert!(t.step(None, 20, true, 200.0, f64::min).is_none());
        assert!(matches!(
            t.step(None, 21, true, 198.0, f64::min),
            Some(Transition::Started)
        ));
        assert_eq!(t.open.as_ref().unwrap().ts_start, 20);

        // Guarda el mínimo y tolera una recuperación breve.
        t.step(None, 22, true, 195.0, f64::min);
        t.step(None, 23, false, 220.0, f64::min);
        t.step(None, 24, true, 199.0, f64::min);
        assert_eq!(t.open.as_ref().unwrap().value, 195.0);

        assert!(t.step(None, 25, false, 220.0, f64::min).is_none());
        assert!(t.step(None, 26, false, 220.0, f64::min).is_none());
        let end = t.step(None, 27, false, 220.0, f64::min);
        // Termina en la primera muestra normal, no en la que confirma.
        assert!(matches!(end, Some(Transition::Ended { duration: 5 })));
        assert!(t.open.is_none());
    }

    #[test]
    fn immediate_tracker_reports_duration() {
        let mut t = Tracker::new(EventKind::Outage, 1, 1);
        assert!(matches!(
            t.step(None, 100, true, 12.6, f64::min),
            Some(Transition::Started)
        ));
        t.step(None, 101, true, 12.1, f64::min);
        let end = t.step(None, 160, false, 13.0, f64::min);
        assert!(matches!(end, Some(Transition::Ended { duration: 60 })));
    }
}
