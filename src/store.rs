//! Historial en SQLite: muestras crudas (1/s), agregados por minuto y eventos.

use std::path::PathBuf;
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::model::Limits;
use crate::protocol::{Flags, Status};

/// Las muestras crudas se conservan este tiempo; los minutos, para siempre.
pub const RAW_RETENTION_SECS: i64 = 7 * 86_400;

const ON_BATT: u8 = Flags::ON_BATTERY;
const AVR: u8 = Flags::AVR_ACTIVE;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS samples (
    ts    INTEGER PRIMARY KEY,
    vin   REAL NOT NULL,
    vout  REAL NOT NULL,
    freq  REAL NOT NULL,
    load  INTEGER NOT NULL,
    batt  REAL NOT NULL,
    flags INTEGER NOT NULL
) WITHOUT ROWID;

-- Las columnas de tensión y frecuencia solo consideran muestras en red.
CREATE TABLE IF NOT EXISTS minutes (
    ts         INTEGER PRIMARY KEY,
    n          INTEGER NOT NULL,
    n_batt     INTEGER NOT NULL,
    n_avr      INTEGER NOT NULL,
    n_good     INTEGER NOT NULL,
    n_marginal INTEGER NOT NULL,
    n_out      INTEGER NOT NULL,
    vin_min    REAL,
    vin_max    REAL,
    vin_avg    REAL,
    freq_min   REAL,
    freq_max   REAL,
    freq_avg   REAL,
    vout_avg   REAL NOT NULL,
    load_avg   REAL NOT NULL,
    load_max   INTEGER NOT NULL,
    batt_min   REAL NOT NULL,
    batt_avg   REAL NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS events (
    id       INTEGER PRIMARY KEY,
    kind     TEXT NOT NULL,
    ts_start INTEGER NOT NULL,
    ts_end   INTEGER,
    value    REAL
);
CREATE INDEX IF NOT EXISTS events_start ON events (ts_start);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Outage,
    Avr,
    Sag,
    Swell,
    BattLow,
    Failure,
}

impl EventKind {
    pub const ALL: [EventKind; 6] = [
        EventKind::Outage,
        EventKind::Avr,
        EventKind::Sag,
        EventKind::Swell,
        EventKind::BattLow,
        EventKind::Failure,
    ];

    pub fn key(self) -> &'static str {
        match self {
            EventKind::Outage => "outage",
            EventKind::Avr => "avr",
            EventKind::Sag => "sag",
            EventKind::Swell => "swell",
            EventKind::BattLow => "batt_low",
            EventKind::Failure => "failure",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            EventKind::Outage => "Corte de luz",
            EventKind::Avr => "AVR activo",
            EventKind::Sag => "Baja tensión",
            EventKind::Swell => "Sobretensión",
            EventKind::BattLow => "Batería baja",
            EventKind::Failure => "Falla del UPS",
        }
    }

    /// Qué representa `Event::value` para este tipo de evento.
    pub fn value_label(self) -> &'static str {
        match self {
            EventKind::Outage | EventKind::BattLow => "batería mín.",
            EventKind::Sag => "mín.",
            EventKind::Swell => "máx.",
            EventKind::Avr | EventKind::Failure => "entrada",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub ts_start: i64,
    /// `None` mientras el evento sigue en curso.
    pub ts_end: Option<i64>,
    pub value: Option<f64>,
}

/// Un punto de serie temporal: una muestra o un balde de varias.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub ts: i64,
    /// Segundos en batería y con el AVR activo dentro del balde.
    pub n_batt: i64,
    pub n_avr: i64,
    pub vin_min: Option<f64>,
    pub vin_max: Option<f64>,
    pub vin_avg: Option<f64>,
    pub freq_avg: Option<f64>,
    pub vout_avg: f64,
    pub load_avg: f64,
    pub batt_min: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    /// Segundos observados (una muestra por segundo).
    pub n: i64,
    pub n_batt: i64,
    pub n_avr: i64,
    pub n_good: i64,
    pub n_marginal: i64,
    pub n_out: i64,
    pub vin_min: Option<f64>,
    pub vin_max: Option<f64>,
    pub vin_avg: Option<f64>,
    pub freq_min: Option<f64>,
    pub freq_max: Option<f64>,
    pub batt_min: Option<f64>,
    pub load_avg: Option<f64>,
    pub load_max: Option<f64>,
}

impl Stats {
    pub fn n_grid(&self) -> i64 {
        self.n - self.n_batt
    }
}

pub struct Store {
    conn: Connection,
}

/// El UPS informa con un decimal; se guarda así y no con el ruido del `f32`.
fn tenths(v: f32) -> f64 {
    (f64::from(v) * 10.0).round() / 10.0
}

/// Carpeta del historial. `UPS_MONITOR_DATA_DIR` permite usar otra (pruebas).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("UPS_MONITOR_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Library/Application Support/ups-monitor")
}

type Result<T> = rusqlite::Result<T>;

impl Store {
    pub fn open() -> Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir).ok();
        Self::from_connection(Connection::open(dir.join("history.db"))?)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // ── Escritura (hilo de sondeo) ──────────────────────────────────────────

    pub fn insert_sample(&self, ts: i64, s: &Status) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO samples (ts, vin, vout, freq, load, batt, flags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ts,
                tenths(s.input_v),
                tenths(s.output_v),
                tenths(s.freq_hz),
                s.load_pct,
                tenths(s.batt_v),
                s.flags.0
            ],
        )?;
        Ok(())
    }

    /// Agrega en `minutes` todos los minutos completos anteriores a `now` y
    /// descarta las muestras crudas vencidas.
    pub fn rollup(&self, now: i64, limits: &Limits) -> Result<()> {
        let until = now - now.rem_euclid(60);
        let from: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(ts) + 60, 0) FROM minutes", [], |r| {
                    r.get(0)
                })?;
        if from < until {
            self.conn.execute(
                &format!(
                    "INSERT OR REPLACE INTO minutes
                     SELECT (ts / 60) * 60 AS m,
                            COUNT(*),
                            SUM({batt}),
                            SUM(flags & {AVR} != 0),
                            SUM({grid} AND vin BETWEEN ?3 AND ?4),
                            SUM({grid} AND vin BETWEEN ?5 AND ?6 AND vin NOT BETWEEN ?3 AND ?4),
                            SUM({grid} AND vin NOT BETWEEN ?5 AND ?6),
                            MIN(CASE WHEN {grid} THEN vin END),
                            MAX(CASE WHEN {grid} THEN vin END),
                            AVG(CASE WHEN {grid} THEN vin END),
                            MIN(CASE WHEN {grid} THEN freq END),
                            MAX(CASE WHEN {grid} THEN freq END),
                            AVG(CASE WHEN {grid} THEN freq END),
                            AVG(vout), AVG(load), MAX(load), MIN(batt), AVG(batt)
                     FROM samples WHERE ts >= ?1 AND ts < ?2 GROUP BY m",
                    batt = format_args!("(flags & {ON_BATT} != 0)"),
                    grid = format_args!("(flags & {ON_BATT} = 0)"),
                ),
                params_from_iter(
                    [from, until]
                        .map(Value::from)
                        .into_iter()
                        .chain(limits.sql_params().map(Value::from)),
                ),
            )?;
        }
        self.conn.execute(
            "DELETE FROM samples WHERE ts < ?1",
            [until - RAW_RETENTION_SECS],
        )?;
        Ok(())
    }

    pub fn open_event(&self, kind: EventKind, ts: i64, value: Option<f64>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (kind, ts_start, value) VALUES (?1, ?2, ?3)",
            params![kind.key(), ts, value],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_event(&self, id: i64, value: Option<f64>) -> Result<()> {
        self.conn.execute(
            "UPDATE events SET value = ?2 WHERE id = ?1",
            params![id, value],
        )?;
        Ok(())
    }

    pub fn close_event(&self, id: i64, ts: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE events SET ts_end = ?2 WHERE id = ?1",
            params![id, ts],
        )?;
        Ok(())
    }

    /// Cierra eventos que quedaron abiertos por un cierre abrupto, usando la
    /// última muestra registrada como fin.
    pub fn close_stale_events(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE events
             SET ts_end = MAX(ts_start, COALESCE((SELECT MAX(ts) FROM samples), ts_start))
             WHERE ts_end IS NULL",
            [],
        )?;
        Ok(())
    }

    // ── Lectura (interfaz y CLI) ────────────────────────────────────────────

    pub fn latest(&self) -> Result<Option<(i64, Status)>> {
        self.conn
            .query_row(
                "SELECT ts, vin, vout, freq, load, batt, flags FROM samples
                 ORDER BY ts DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        Status {
                            input_v: r.get::<_, f64>(1)? as f32,
                            fault_v: 0.0,
                            output_v: r.get::<_, f64>(2)? as f32,
                            freq_hz: r.get::<_, f64>(3)? as f32,
                            load_pct: r.get(4)?,
                            batt_v: r.get::<_, f64>(5)? as f32,
                            temp_c: None,
                            flags: Flags(r.get(6)?),
                        },
                    ))
                },
            )
            .optional()
    }

    /// Serie entre `from` y `to` en baldes de `bucket` segundos. Usa las
    /// muestras crudas si `raw`, o los agregados por minuto en caso contrario.
    pub fn series(&self, from: i64, to: i64, bucket: i64, raw: bool) -> Result<Vec<Point>> {
        let bucket = bucket.max(1);
        let sql = if raw {
            format!(
                "SELECT (ts / ?3) * ?3 AS t,
                        SUM({batt}),
                        SUM(flags & {AVR} != 0),
                        MIN(CASE WHEN {grid} THEN vin END),
                        MAX(CASE WHEN {grid} THEN vin END),
                        AVG(CASE WHEN {grid} THEN vin END),
                        AVG(CASE WHEN {grid} THEN freq END),
                        AVG(vout), AVG(load), MIN(batt)
                 FROM samples WHERE ts >= ?1 AND ts <= ?2 GROUP BY t ORDER BY t",
                batt = format_args!("(flags & {ON_BATT} != 0)"),
                grid = format_args!("(flags & {ON_BATT} = 0)"),
            )
        } else {
            "SELECT (ts / ?3) * ?3 AS t,
                    SUM(n_batt), SUM(n_avr),
                    MIN(vin_min), MAX(vin_max),
                    SUM(vin_avg * (n - n_batt)) / SUM(CASE WHEN vin_avg IS NOT NULL THEN n - n_batt END),
                    SUM(freq_avg * (n - n_batt)) / SUM(CASE WHEN freq_avg IS NOT NULL THEN n - n_batt END),
                    SUM(vout_avg * n) / SUM(n), SUM(load_avg * n) / SUM(n), MIN(batt_min)
             FROM minutes WHERE ts >= ?1 AND ts <= ?2
             GROUP BY t ORDER BY t"
                .to_owned()
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([from, to, bucket], |r| {
            Ok(Point {
                ts: r.get(0)?,
                n_batt: r.get(1)?,
                n_avr: r.get(2)?,
                vin_min: r.get(3)?,
                vin_max: r.get(4)?,
                vin_avg: r.get(5)?,
                freq_avg: r.get(6)?,
                vout_avg: r.get(7)?,
                load_avg: r.get(8)?,
                batt_min: r.get(9)?,
            })
        })?;
        rows.collect()
    }

    pub fn stats(&self, from: i64, to: i64, raw: bool, limits: &Limits) -> Result<Stats> {
        let sql = if raw {
            format!(
                "SELECT COUNT(*),
                        SUM({batt}),
                        SUM(flags & {AVR} != 0),
                        SUM({grid} AND vin BETWEEN ?3 AND ?4),
                        SUM({grid} AND vin BETWEEN ?5 AND ?6 AND vin NOT BETWEEN ?3 AND ?4),
                        SUM({grid} AND vin NOT BETWEEN ?5 AND ?6),
                        MIN(CASE WHEN {grid} THEN vin END),
                        MAX(CASE WHEN {grid} THEN vin END),
                        AVG(CASE WHEN {grid} THEN vin END),
                        MIN(CASE WHEN {grid} THEN freq END),
                        MAX(CASE WHEN {grid} THEN freq END),
                        MIN(batt), AVG(load), MAX(load)
                 FROM samples WHERE ts >= ?1 AND ts <= ?2",
                batt = format_args!("(flags & {ON_BATT} != 0)"),
                grid = format_args!("(flags & {ON_BATT} = 0)"),
            )
        } else {
            "SELECT SUM(n), SUM(n_batt), SUM(n_avr), SUM(n_good), SUM(n_marginal), SUM(n_out),
                    MIN(vin_min), MAX(vin_max),
                    SUM(vin_avg * (n - n_batt)) / SUM(CASE WHEN vin_avg IS NOT NULL THEN n - n_batt END),
                    MIN(freq_min), MAX(freq_max),
                    MIN(batt_min), SUM(load_avg * n) / SUM(n), MAX(load_max)
             FROM minutes WHERE ts >= ?1 AND ts <= ?2"
                .to_owned()
        };
        let count = |r: &Row, i: usize| r.get::<_, Option<i64>>(i).map(Option::unwrap_or_default);
        let bands = limits.sql_params();
        let bands: &[f64] = if raw { &bands } else { &[] };
        let args = [from, to].map(Value::from).into_iter();
        self.conn.prepare_cached(&sql)?.query_row(
            params_from_iter(args.chain(bands.iter().copied().map(Value::from))),
            |r| {
                Ok(Stats {
                    n: count(r, 0)?,
                    n_batt: count(r, 1)?,
                    n_avr: count(r, 2)?,
                    n_good: count(r, 3)?,
                    n_marginal: count(r, 4)?,
                    n_out: count(r, 5)?,
                    vin_min: r.get(6)?,
                    vin_max: r.get(7)?,
                    vin_avg: r.get(8)?,
                    freq_min: r.get(9)?,
                    freq_max: r.get(10)?,
                    batt_min: r.get(11)?,
                    load_avg: r.get(12)?,
                    load_max: r.get(13)?,
                })
            },
        )
    }

    /// Distribución de la tensión de entrada: (voltios redondeados, segundos).
    pub fn histogram(&self, from: i64, to: i64, raw: bool) -> Result<Vec<(i64, i64)>> {
        let sql = if raw {
            format!(
                "SELECT CAST(ROUND(vin) AS INTEGER) AS v, COUNT(*) FROM samples
                 WHERE ts >= ?1 AND ts <= ?2 AND flags & {ON_BATT} = 0 GROUP BY v ORDER BY v"
            )
        } else {
            "SELECT CAST(ROUND(vin_avg) AS INTEGER) AS v, SUM(n - n_batt) FROM minutes
             WHERE ts >= ?1 AND ts <= ?2 AND vin_avg IS NOT NULL GROUP BY v ORDER BY v"
                .to_owned()
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map([from, to], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Eventos que se solapan con `[from, to]`, del más reciente al más antiguo.
    pub fn events(&self, from: i64, to: i64, limit: i64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT kind, ts_start, ts_end, value FROM events
             WHERE ts_start <= ?2 AND (ts_end IS NULL OR ts_end >= ?1)
             ORDER BY ts_start DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map([from, to, limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (kind, ts_start, ts_end, value) = row?;
            if let Some(kind) = EventKind::from_key(&kind) {
                events.push(Event {
                    kind,
                    ts_start,
                    ts_end,
                    value,
                });
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parse_status;

    /// Inicio de minuto arbitrario (múltiplo de 60).
    const T0: i64 = 1_800_000_000;
    const GRID: &str = "(221.0 221.0 222.0 010 50.0 13.6 --.- 00001001";
    const HIGH: &str = "(234.0 221.0 222.0 020 50.1 13.6 --.- 00101001";
    const OUTAGE: &str = "(000.0 221.0 220.0 030 00.0 12.2 --.- 10001001";

    fn fill(store: &Store, from: i64, count: i64, line: &str) {
        let s = parse_status(line).unwrap();
        for ts in from..from + count {
            store.insert_sample(ts, &s).unwrap();
        }
    }

    #[test]
    fn rollup_matches_raw_stats() {
        let store = Store::in_memory().unwrap();
        let limits = Limits::new(220.0);
        let t0 = T0;
        fill(&store, t0, 60, GRID);
        fill(&store, t0 + 60, 30, HIGH);
        fill(&store, t0 + 90, 30, OUTAGE);
        fill(&store, t0 + 120, 10, GRID); // minuto incompleto: no se agrega

        store.rollup(t0 + 130, &limits).unwrap();

        let raw = store.stats(t0, t0 + 119, true, &limits).unwrap();
        let agg = store.stats(t0, t0 + 119, false, &limits).unwrap();
        for s in [raw, agg] {
            assert_eq!(s.n, 120);
            assert_eq!(s.n_batt, 30);
            assert_eq!(s.n_avr, 30);
            assert_eq!(s.n_good, 60);
            assert_eq!(s.n_marginal, 30);
            assert_eq!(s.n_out, 0);
            assert_eq!(s.n_grid(), 90);
            assert_eq!(s.vin_min, Some(221.0));
            assert_eq!(s.vin_max, Some(234.0));
            assert!((s.vin_avg.unwrap() - (221.0 * 60.0 + 234.0 * 30.0) / 90.0).abs() < 1e-6);
            assert_eq!(s.freq_min, Some(50.0));
            assert_eq!(s.batt_min, Some(12.2));
            assert_eq!(s.load_max, Some(30.0));
        }

        // Volver a agregar no duplica ni pisa minutos ya cerrados.
        store.rollup(t0 + 131, &limits).unwrap();
        assert_eq!(store.stats(t0, t0 + 119, false, &limits).unwrap().n, 120);
    }

    #[test]
    fn series_buckets_and_skips_outage_voltage() {
        let store = Store::in_memory().unwrap();
        let limits = Limits::new(220.0);
        let t0 = T0;
        fill(&store, t0, 60, GRID);
        fill(&store, t0 + 60, 60, OUTAGE);
        store.rollup(t0 + 120, &limits).unwrap();

        for raw in [true, false] {
            let points = store.series(t0, t0 + 119, 60, raw).unwrap();
            assert_eq!(points.len(), 2);
            assert_eq!(points[0].vin_avg, Some(221.0));
            assert_eq!(points[0].n_batt, 0);
            assert_eq!(points[1].vin_avg, None);
            assert_eq!(points[1].freq_avg, None);
            assert_eq!(points[1].n_batt, 60);
        }
        let hist = store.histogram(t0, t0 + 119, true).unwrap();
        assert_eq!(hist, vec![(221, 60)]);
        assert_eq!(store.histogram(t0, t0 + 119, false).unwrap(), hist);
    }

    #[test]
    fn empty_range_is_all_zero() {
        let store = Store::in_memory().unwrap();
        let limits = Limits::new(220.0);
        for raw in [true, false] {
            let s = store.stats(0, 100, raw, &limits).unwrap();
            assert_eq!(s.n, 0);
            assert_eq!(s.vin_avg, None);
            assert!(store.series(0, 100, 10, raw).unwrap().is_empty());
        }
        assert!(store.latest().unwrap().is_none());
    }

    #[test]
    fn prunes_old_raw_samples() {
        let store = Store::in_memory().unwrap();
        let limits = Limits::new(220.0);
        let now = 1_800_000_000;
        fill(&store, now - RAW_RETENTION_SECS - 600, 60, GRID);
        fill(&store, now - 120, 60, GRID);
        store.rollup(now, &limits).unwrap();
        let (oldest,): (i64,) = store
            .conn
            .query_row("SELECT MIN(ts) FROM samples", [], |r| Ok((r.get(0)?,)))
            .unwrap();
        assert_eq!(oldest, now - 120);
        // Lo podado sigue disponible en los agregados.
        assert_eq!(store.stats(0, now, false, &limits).unwrap().n, 120);
    }

    #[test]
    fn event_lifecycle() {
        let store = Store::in_memory().unwrap();
        let id = store
            .open_event(EventKind::Outage, 100, Some(12.5))
            .unwrap();
        store.open_event(EventKind::Sag, 500, None).unwrap();
        store.update_event(id, Some(11.9)).unwrap();
        store.close_event(id, 160).unwrap();

        let all = store.events(0, 1_000, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].kind, EventKind::Sag);
        assert_eq!(all[0].ts_end, None);
        assert_eq!(all[1].kind, EventKind::Outage);
        assert_eq!(all[1].ts_end, Some(160));
        assert_eq!(all[1].value, Some(11.9));
        // Un evento cerrado antes del rango no aparece; uno abierto sí.
        assert_eq!(store.events(200, 1_000, 10).unwrap().len(), 1);

        fill(&store, 500, 20, GRID);
        store.close_stale_events().unwrap();
        assert_eq!(store.events(0, 1_000, 10).unwrap()[0].ts_end, Some(519));
    }
}
