#!/usr/bin/env python3
"""App de barra de menú para monitorear el Lyonn CTB-800V."""

import rumps
import hid
import time
import os
import queue
import threading
from datetime import datetime

VENDOR_ID  = 0x0665
PRODUCT_ID = 0x5161
LOG_FILE   = os.path.expanduser("~/Library/Logs/ups-monitor.log")
POLL_SEC   = 5

# ── Protocolo HID ────────────────────────────────────────────────────────────
def _query() -> dict | None:
    devs = hid.enumerate(VENDOR_ID, PRODUCT_ID)
    if not devs:
        return None
    try:
        with hid.Device(VENDOR_ID, PRODUCT_ID) as dev:
            cmd = b"QS\r"
            padded = cmd + b'\x00' * (8 - len(cmd))
            dev.write(b'\x00' + padded[:8])
            time.sleep(0.15)

            buf = b""
            for _ in range(10):
                chunk = dev.read(8, 500)
                if not chunk:
                    break
                buf += bytes(chunk)
                if b'\r' in buf:
                    break

        raw = buf.split(b'\r')[0]
        if not raw or raw[0:1] != b'(':
            return None

        parts = raw[1:].decode("ascii", errors="ignore").split()
        flags = parts[7] if len(parts) >= 8 else "00000000"
        return {
            "input_v":    float(parts[0]),
            "output_v":   float(parts[2]),
            "load_pct":   int(parts[3]),
            "freq_hz":    float(parts[4]),
            "batt_v":     float(parts[5]),
            "on_battery": flags[0] == '1',
            "batt_low":   flags[1] == '1',
            "avr_active": flags[2] == '1',
            "ups_failed": flags[3] == '1',
        }
    except Exception:
        return None

def _batt_pct(v: float) -> int:
    if v >= 13.5: return 100
    if v >= 13.0: return 90
    if v >= 12.8: return 75
    if v >= 12.5: return 60
    if v >= 12.2: return 40
    if v >= 12.0: return 20
    return 5

def _log(msg: str):
    os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
    ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    with open(LOG_FILE, "a") as f:
        f.write(f"{ts} {msg}\n")

# ── App ───────────────────────────────────────────────────────────────────────
class UPSMenuBar(rumps.App):
    def __init__(self):
        super().__init__("UPS", title="⚡ —", quit_button=None)

        self._prev_on_batt = None
        self._connected    = False

        # Ítems del menú
        self.status_item   = rumps.MenuItem("Conectando...")
        self.input_item    = rumps.MenuItem("")
        self.output_item   = rumps.MenuItem("")
        self.freq_item     = rumps.MenuItem("")
        self.load_item     = rumps.MenuItem("")
        self.batt_item     = rumps.MenuItem("")
        self.updated_item  = rumps.MenuItem("")
        self.sep1          = rumps.separator
        self.log_item      = rumps.MenuItem("Ver log", callback=self.open_log)
        self.quit_item     = rumps.MenuItem("Salir", callback=rumps.quit_application)

        self.menu = [
            self.status_item,
            rumps.separator,
            self.input_item,
            self.output_item,
            self.freq_item,
            self.load_item,
            self.batt_item,
            rumps.separator,
            self.updated_item,
            rumps.separator,
            self.log_item,
            self.quit_item,
        ]

        # Hilo de polling en background: solo consulta el HID y encola el resultado.
        # Toda escritura de UI (AppKit) va por el timer, que corre en el hilo principal.
        self._results = queue.Queue()
        self._ui_timer = rumps.Timer(self._drain, 1)
        self._ui_timer.start()

        t = threading.Thread(target=self._poll_loop, daemon=True)
        t.start()

    def _poll_loop(self):
        while True:
            self._results.put(_query())
            time.sleep(POLL_SEC)

    def _drain(self, _):
        while True:
            try:
                s = self._results.get_nowait()
            except queue.Empty:
                return
            self._apply(s)

    def _apply(self, s):
        if s is None:
            self._connected = False
            self.title = "⚡ ?"
            self.status_item.title = "UPS no detectado"
            self.input_item.title  = ""
            self.output_item.title = ""
            self.freq_item.title   = ""
            self.load_item.title   = ""
            self.batt_item.title   = ""
            self.updated_item.title = f"Última actualización: {datetime.now().strftime('%H:%M:%S')}"
            return

        self._connected = True
        pct = _batt_pct(s["batt_v"])

        # Ícono en la barra según estado
        if s["ups_failed"]:
            icon = "❌"
        elif s["on_battery"]:
            icon = "🔋"
        elif s["avr_active"]:
            icon = "⚠️"
        else:
            icon = "⚡"

        self.title = f"{icon} {pct}%"

        # Estado principal
        if s["on_battery"]:
            status = "EN BATERÍA — se fue la luz"
        elif s["ups_failed"]:
            status = "FALLA EN UPS"
        elif s["avr_active"]:
            status = "En red (AVR activo)"
        else:
            status = "En red eléctrica — OK"

        self.status_item.title  = f"Estado: {status}"
        self.input_item.title   = f"Entrada:  {s['input_v']:.1f} V"
        self.output_item.title  = f"Salida:   {s['output_v']:.1f} V"
        self.freq_item.title    = f"Freq:     {s['freq_hz']:.1f} Hz"
        self.load_item.title    = f"Carga:    {s['load_pct']} %"
        self.batt_item.title    = f"Batería:  {s['batt_v']:.1f} V  ({pct}%)"
        self.updated_item.title = f"Actualizado: {datetime.now().strftime('%H:%M:%S')}"

        # Notificaciones por cambio de estado
        on_batt = s["on_battery"]
        if self._prev_on_batt is not None and on_batt != self._prev_on_batt:
            if on_batt:
                rumps.notification(
                    title="UPS — Se fue la luz",
                    subtitle="",
                    message="El UPS cambió a batería. Guardá tu trabajo.",
                    sound=True,
                )
                _log("ALERTA: UPS en batería")
            else:
                rumps.notification(
                    title="UPS — Luz restaurada",
                    subtitle="",
                    message="El UPS volvió a la red eléctrica.",
                    sound=True,
                )
                _log("OK: UPS en línea")

        if on_batt and s["batt_low"]:
            rumps.notification(
                title="UPS — Batería crítica",
                subtitle="",
                message="Batería muy baja. Apagá el equipo ahora.",
                sound=True,
            )
            _log("CRITICO: Batería baja")

        self._prev_on_batt = on_batt

    def open_log(self, _):
        if os.path.exists(LOG_FILE):
            rumps.alert(
                title="Log UPS",
                message=open(LOG_FILE).read()[-2000:] or "(vacío)",
            )
        else:
            rumps.alert("Log vacío", "Aún no hay eventos registrados.")

if __name__ == "__main__":
    UPSMenuBar().run()
