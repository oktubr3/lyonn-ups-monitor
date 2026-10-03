//! Gráfico de series temporales dibujado directamente con el `Painter`.

use chrono::{Datelike, Local, Offset, TimeZone};
use eframe::egui::{Align2, Color32, Mesh, Pos2, Rect, Sense, Shape, Stroke, Ui, pos2, vec2};

use crate::model::num;
use crate::theme::{self, font};

const HEADER_H: f32 = 22.0;
const AXIS_W: f32 = 44.0;
const AXIS_H: f32 = 18.0;
const TICK_STEPS: [i64; 15] = [
    60, 120, 300, 600, 900, 1_800, 3_600, 7_200, 10_800, 21_600, 43_200, 86_400, 172_800, 604_800,
    1_209_600,
];

pub struct Series {
    pub name: &'static str,
    pub color: Color32,
    /// Un valor por punto del eje X; `NaN` corta la línea.
    pub ys: Vec<f64>,
    /// Mínimo y máximo de cada balde, para sombrear la dispersión.
    pub envelope: Option<(Vec<f64>, Vec<f64>)>,
}

pub struct Chart<'a> {
    pub title: &'a str,
    pub unit: &'a str,
    pub decimals: usize,
    pub from: i64,
    pub to: i64,
    /// Segundos entre puntos consecutivos; un salto mayor corta la línea.
    pub bucket: i64,
    pub xs: &'a [i64],
    pub series: Vec<Series>,
    /// Valores que el eje Y siempre debe abarcar.
    pub y_include: Vec<f64>,
    pub y_min_span: f64,
    /// Franjas horizontales de referencia: (desde, hasta, color).
    pub bands: Vec<(f64, f64, Color32)>,
    /// Líneas de referencia punteadas.
    pub refs: Vec<f64>,
    /// Intervalos resaltados en toda la altura (cortes de luz).
    pub spans: &'a [(i64, i64)],
    pub height: f32,
}

pub fn local_offset(ts: i64) -> i64 {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map_or(0, |t| i64::from(t.offset().fix().local_minus_utc()))
}

pub fn format_ts(ts: i64, fmt: &str) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map_or_else(String::new, |t| t.format(fmt).to_string())
}

/// Día de la semana y fecha, p. ej. `vie 03/10`.
pub fn day_label(ts: i64) -> String {
    const DAYS: [&str; 7] = ["lun", "mar", "mié", "jue", "vie", "sáb", "dom"];
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map_or_else(String::new, |t| {
            let day = DAYS[t.weekday().num_days_from_monday() as usize];
            format!("{day} {}", t.format("%d/%m"))
        })
}

/// Paso "redondo" (1, 2, 5 × 10ⁿ) más cercano por encima de `raw`.
fn nice_step(raw: f64) -> f64 {
    let mag = 10f64.powf(raw.max(1e-9).log10().floor());
    let norm = raw / mag;
    let nice = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

/// Índice del punto más cercano a `ts`, si está a menos de `tolerance`.
fn nearest(xs: &[i64], ts: i64, tolerance: i64) -> Option<usize> {
    if xs.is_empty() {
        return None;
    }
    let i = xs.partition_point(|&x| x < ts);
    let best = [i.checked_sub(1), (i < xs.len()).then_some(i)]
        .into_iter()
        .flatten()
        .min_by_key(|&j| (xs[j] - ts).abs())?;
    ((xs[best] - ts).abs() <= tolerance).then_some(best)
}

impl Chart<'_> {
    fn y_range(&self) -> (f64, f64) {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        let mut extend = |v: f64| {
            if v.is_finite() {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        };
        for s in &self.series {
            s.ys.iter().copied().for_each(&mut extend);
            if let Some((mins, maxs)) = &s.envelope {
                mins.iter().chain(maxs).copied().for_each(&mut extend);
            }
        }
        self.y_include.iter().copied().for_each(&mut extend);
        if !lo.is_finite() {
            (lo, hi) = (0.0, 1.0);
        }
        if hi - lo < self.y_min_span {
            let mid = (hi + lo) / 2.0;
            (lo, hi) = (mid - self.y_min_span / 2.0, mid + self.y_min_span / 2.0);
        }
        let pad = (hi - lo) * 0.08;
        (lo - pad, hi + pad)
    }

    /// Dibuja el gráfico. `hover` es el instante bajo el cursor en cualquiera
    /// de los gráficos enlazados; devuelve el propio si el cursor está encima.
    pub fn show(self, ui: &mut Ui, hover: Option<i64>) -> Option<i64> {
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), Sense::hover());
        let painter = ui.painter_at(rect);
        let plot = Rect::from_min_max(
            pos2(rect.left() + AXIS_W, rect.top() + HEADER_H),
            pos2(rect.right() - 4.0, rect.bottom() - AXIS_H),
        );

        let (y_lo, y_hi) = self.y_range();
        let span_x = (self.to - self.from).max(1) as f64;
        let x_of = |ts: i64| plot.left() + ((ts - self.from) as f64 / span_x) as f32 * plot.width();
        let y_of = |v: f64| plot.bottom() - ((v - y_lo) / (y_hi - y_lo)) as f32 * plot.height();
        let clip = painter.with_clip_rect(plot);

        // Franjas de referencia y cortes.
        for &(lo, hi, color) in &self.bands {
            let band =
                Rect::from_min_max(pos2(plot.left(), y_of(hi)), pos2(plot.right(), y_of(lo)));
            clip.rect_filled(band, 0.0, color);
        }
        for &(t0, t1) in self.spans {
            let x0 = x_of(t0.max(self.from));
            let x1 = x_of(t1.min(self.to)).max(x0 + 1.5);
            clip.rect_filled(
                Rect::from_min_max(pos2(x0, plot.top()), pos2(x1, plot.bottom())),
                0.0,
                theme::alpha(theme::CRITICAL, 70),
            );
        }

        // Grilla y eje Y.
        let step = nice_step((y_hi - y_lo) / 4.0);
        let mut tick = (y_lo / step).ceil() * step;
        while tick <= y_hi {
            let y = y_of(tick);
            painter.hline(plot.x_range(), y, Stroke::new(1.0, theme::GRID));
            // Una marca pegada al borde superior chocaría con el título.
            if y > plot.top() + 5.0 {
                painter.text(
                    pos2(plot.left() - 6.0, y),
                    Align2::RIGHT_CENTER,
                    num(tick, usize::from(step < 1.0)),
                    font(10.5),
                    theme::MUTED,
                );
            }
            tick += step;
        }
        for &y in &self.refs {
            clip.extend(Shape::dashed_line(
                &[pos2(plot.left(), y_of(y)), pos2(plot.right(), y_of(y))],
                Stroke::new(1.0, theme::AXIS),
                4.0,
                4.0,
            ));
        }

        // Eje X: marcas alineadas a horas locales redondas.
        let max_ticks = (plot.width() / 84.0).max(2.0) as i64;
        let x_step = TICK_STEPS
            .into_iter()
            .find(|s| (self.to - self.from) / s <= max_ticks)
            .unwrap_or(2_592_000);
        let offset = local_offset(self.to);
        let mut t = self.from - (self.from + offset).rem_euclid(x_step) + x_step;
        while t <= self.to {
            let x = x_of(t);
            painter.vline(
                x,
                plot.y_range(),
                Stroke::new(1.0, theme::alpha(theme::GRID, 140)),
            );
            let midnight = (t + offset).rem_euclid(86_400) == 0;
            let label = format_ts(t, if midnight { "%d/%m" } else { "%H:%M" });
            painter.text(
                pos2(x, plot.bottom() + 4.0),
                Align2::CENTER_TOP,
                label,
                font(10.5),
                theme::MUTED,
            );
            t += x_step;
        }
        painter.hline(plot.x_range(), plot.bottom(), Stroke::new(1.0, theme::AXIS));

        // Series.
        let gap = self.bucket * 5 / 2 + 2;
        for s in &self.series {
            if let Some((mins, maxs)) = &s.envelope {
                let mut mesh = Mesh::default();
                let fill = theme::alpha(s.color, 46);
                for i in 1..self.xs.len() {
                    let vals = [mins[i - 1], maxs[i - 1], mins[i], maxs[i]];
                    if self.xs[i] - self.xs[i - 1] > gap || vals.iter().any(|v| !v.is_finite()) {
                        continue;
                    }
                    let (x0, x1) = (x_of(self.xs[i - 1]), x_of(self.xs[i]));
                    let base = mesh.vertices.len() as u32;
                    mesh.colored_vertex(pos2(x0, y_of(vals[0])), fill);
                    mesh.colored_vertex(pos2(x0, y_of(vals[1])), fill);
                    mesh.colored_vertex(pos2(x1, y_of(vals[2])), fill);
                    mesh.colored_vertex(pos2(x1, y_of(vals[3])), fill);
                    mesh.add_triangle(base, base + 1, base + 2);
                    mesh.add_triangle(base + 1, base + 2, base + 3);
                }
                clip.add(Shape::mesh(mesh));
            }

            let stroke = Stroke::new(1.75, s.color);
            let mut run: Vec<Pos2> = Vec::new();
            let flush = |run: &mut Vec<Pos2>| {
                match run.len() {
                    0 => {}
                    1 => {
                        clip.circle_filled(run[0], 1.75, s.color);
                    }
                    _ => {
                        clip.line(std::mem::take(run), stroke);
                    }
                }
                run.clear();
            };
            for (i, (&x, &y)) in self.xs.iter().zip(&s.ys).enumerate() {
                if !y.is_finite() || (i > 0 && x - self.xs[i - 1] > gap) {
                    flush(&mut run);
                }
                if y.is_finite() {
                    run.push(pos2(x_of(x), y_of(y)));
                }
            }
            flush(&mut run);
        }

        // Cursor: propio o el de un gráfico enlazado.
        let own_hover = response
            .hover_pos()
            .filter(|p| plot.x_range().contains(p.x))
            .map(|p| self.from + (f64::from((p.x - plot.left()) / plot.width()) * span_x) as i64);
        let cursor = own_hover.or(hover);
        let at = cursor.and_then(|ts| nearest(self.xs, ts, gap.max(self.bucket * 2)));
        if let Some(ts) = cursor {
            let x = at.map_or_else(|| x_of(ts), |i| x_of(self.xs[i]));
            painter.vline(x, plot.y_range(), Stroke::new(1.0, theme::MUTED));
        }
        if let Some(i) = at {
            for s in &self.series {
                if s.ys[i].is_finite() {
                    let p = pos2(x_of(self.xs[i]), y_of(s.ys[i]));
                    clip.circle_filled(p, 4.5, theme::SURFACE);
                    clip.circle_filled(p, 3.0, s.color);
                }
            }
        }

        // Encabezado: título a la izquierda, lectura a la derecha.
        let head_y = rect.top() + HEADER_H / 2.0 - 2.0;
        painter.text(
            pos2(rect.left() + 2.0, head_y),
            Align2::LEFT_CENTER,
            self.title,
            font(13.0),
            theme::INK_2,
        );
        let mut right = rect.right() - 4.0;
        let readout_at = if cursor.is_some() {
            at
        } else {
            self.xs.len().checked_sub(1)
        };
        for s in self.series.iter().rev() {
            let value = match readout_at {
                // Sin cursor se muestra el último valor válido de cada serie.
                Some(i) if cursor.is_none() => {
                    s.ys[..=i].iter().rev().copied().find(|v| v.is_finite())
                }
                Some(i) => Some(s.ys[i]).filter(|v| v.is_finite()),
                None => None,
            };
            let value = value.map_or("—".to_owned(), |v| {
                format!("{} {}", num(v, self.decimals), self.unit)
            });
            let text = if self.series.len() > 1 {
                format!("{}  {value}", s.name)
            } else {
                value
            };
            let r = painter.text(
                pos2(right, head_y),
                Align2::RIGHT_CENTER,
                text,
                font(12.5),
                theme::INK,
            );
            painter.circle_filled(pos2(r.left() - 9.0, head_y), 4.0, s.color);
            right = r.left() - 26.0;
        }
        if let Some(ts) = cursor {
            let ts = at.map_or(ts, |i| self.xs[i]);
            let fmt = if self.to - self.from > 86_400 {
                "%d/%m %H:%M"
            } else {
                "%H:%M:%S"
            };
            painter.text(
                pos2(right, head_y),
                Align2::RIGHT_CENTER,
                format_ts(ts, fmt),
                font(12.0),
                theme::MUTED,
            );
        }

        own_hover
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_steps() {
        assert_eq!(nice_step(0.07), 0.1);
        assert_eq!(nice_step(1.3), 2.0);
        assert_eq!(nice_step(3.0), 5.0);
        assert_eq!(nice_step(7.0), 10.0);
        assert_eq!(nice_step(40.0), 50.0);
    }

    #[test]
    fn nearest_respects_tolerance() {
        let xs = [10, 20, 30, 100];
        assert_eq!(nearest(&xs, 21, 5), Some(1));
        assert_eq!(nearest(&xs, 26, 5), Some(2));
        assert_eq!(nearest(&xs, 60, 5), None);
        assert_eq!(nearest(&xs, 500, 1_000), Some(3));
        assert_eq!(nearest(&xs, 0, 10), Some(0));
        assert_eq!(nearest(&[], 5, 10), None);
    }
}
