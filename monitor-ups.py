#!/usr/bin/env python3
"""Monitor para Lyonn CTB-800V — protocolo Megatec QS via USB HID."""

import hid
import time
import os
import sys
import subprocess
import signal
from datetime import datetime

VENDOR_ID  = 0x0665
PRODUCT_ID = 0x5161
LOG_FILE   = os.path.expanduser("~/Library/Logs/ups-monitor.log")

# ── ANSI ────────────────────────────────────────────────────────────────────
RED    = "\033[0;31m"
YELLOW = "\033[1;33m"
GREEN  = "\033[0;32m"
CYAN   = "\033[0;36m"
BOLD   = "\033[1m"
NC     = "\033[0m"

# ── Protocolo ────────────────────────────────────────────────────────────────
def _open() -> hid.Device:
    devs = hid.enumerate(VENDOR_ID, PRODUCT_ID)
    if not devs:
        sys.exit(f"{RED}UPS no encontrado. Verificá el cable USB.{NC}")
    return hid.Device(VENDOR_ID, PRODUCT_ID)

def _send(dev: hid.Device, cmd: bytes):
    padded = cmd + b'\x00' * (8 - len(cmd))
    dev.write(b'\x00' + padded[:8])

def _recv(dev: hid.Device, chunks: int = 10, timeout_ms: int = 500) -> bytes:
    buf = b""
    for _ in range(chunks):
        chunk = dev.read(8, timeout_ms)
        if not chunk:
            break
        buf += bytes(chunk)
        if b'\r' in buf:
            break
    return buf.split(b'\r')[0]

def query_status(dev: hid.Device) -> dict | None:
    """Envía QS y parsea la respuesta Megatec."""
    _send(dev, b"QS\r")
    time.sleep(0.15)
    raw = _recv(dev)
    if not raw or raw[0:1] != b'(':
        return None
    try:
        parts = raw[1:].decode("ascii", errors="ignore").split()
        flags = parts[7] if len(parts) >= 8 else "00000000"
        return {
            "input_v":      float(parts[0]),
            "output_v":     float(parts[2]),
            "load_pct":     int(parts[3]),
            "freq_hz":      float(parts[4]),
            "batt_v":       float(parts[5]),
            "on_battery":   flags[0] == '1',
            "batt_low":     flags[1] == '1',
            "avr_active":   flags[2] == '1',
            "ups_failed":   flags[3] == '1',
            "standby_mode": flags[4] == '1',
            "test_mode":    flags[5] == '1',
            "shutdown_act": flags[6] == '1',
            "beeper_on":    flags[7] == '1',
        }
    except (IndexError, ValueError):
        return None

def batt_pct(volts: float) -> int:
    if volts >= 13.5: return 100
    if volts >= 13.0: return 90
    if volts >= 12.8: return 75
    if volts >= 12.5: return 60
    if volts >= 12.2: return 40
    if volts >= 12.0: return 20
    return 5

def runtime_estimate(bv: float, load: int) -> str:
    pct = batt_pct(bv)
    if load == 0:
        return "- (sin carga)"
    wh = 84 * (pct / 100)       # 12V 7Ah ≈ 84Wh, ~70% eficiencia efectiva
    w_load = 480 * (load / 100) # 800VA ≈ 480W reales
    mins = int((wh / w_load) * 60)
    return f"~{mins} min"

# ── Notificaciones macOS ─────────────────────────────────────────────────────
def notify(title: str, msg: str):
    script = f'display notification "{msg}" with title "{title}" sound name "Basso"'
    subprocess.run(["osascript", "-e", script], capture_output=True)

def log(msg: str):
    os.makedirs(os.path.dirname(LOG_FILE), exist_ok=True)
    ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    with open(LOG_FILE, "a") as f:
        f.write(f"{ts} {msg}\n")

# ── Pantalla ─────────────────────────────────────────────────────────────────
def draw(s: dict):
    pct    = batt_pct(s["batt_v"])
    filled = pct // 5
    empty  = 20 - filled

    if s["on_battery"]:
        status_str = f"{RED}● EN BATERÍA (corte de luz){NC}"
    elif s["ups_failed"]:
        status_str = f"{RED}● FALLA EN UPS{NC}"
    elif s["avr_active"]:
        status_str = f"{YELLOW}● En red (AVR activo){NC}"
    else:
        status_str = f"{GREEN}● En red eléctrica — OK{NC}"

    bar_color = GREEN if pct >= 60 else (YELLOW if pct >= 30 else RED)
    bar = bar_color + "█" * filled + NC + "░" * empty

    print("\033[2J\033[H", end="")  # clear screen sin os.system
    print(f"{BOLD}══════════════════════════════════════════{NC}")
    print(f"{BOLD}  Monitor UPS — Lyonn CTB-800V{NC}")
    print(f"{BOLD}══════════════════════════════════════════{NC}")
    print()
    print(f"  Estado:        {status_str}")
    print()
    print(f"  {'Tensión entrada:':20s}  {s['input_v']:.1f} V")
    print(f"  {'Tensión salida:':20s}  {s['output_v']:.1f} V")
    print(f"  {'Frecuencia:':20s}  {s['freq_hz']:.1f} Hz")
    print(f"  {'Carga actual:':20s}  {s['load_pct']} %")
    print()
    print(f"  {'Batería:':20s}  [{bar}] {pct}%")
    print(f"  {'Voltaje batería:':20s}  {s['batt_v']:.1f} V")
    print(f"  {'Autonomía est.:':20s}  {runtime_estimate(s['batt_v'], s['load_pct'])}")
    print()

    flags = []
    if s["avr_active"]:   flags.append("AVR")
    if s["test_mode"]:    flags.append("TEST")
    if s["shutdown_act"]: flags.append("SHUTDOWN")
    if s["beeper_on"]:    flags.append("BEEPER")
    if s["standby_mode"]: flags.append("STANDBY")
    if flags:
        print(f"  Flags: {', '.join(flags)}")
        print()

    print(f"{BOLD}══════════════════════════════════════════{NC}")
    print(f"  {datetime.now().strftime('%H:%M:%S')}  — Ctrl+C para salir")
    print()

# ── Modos ────────────────────────────────────────────────────────────────────
def cmd_status():
    with _open() as dev:
        s = query_status(dev)
    if not s:
        print(f"{RED}No se pudo leer el UPS.{NC}")
        return
    draw(s)

def cmd_monitor(interval: int = 5):
    prev_on_batt = None

    def _exit(sig, frame):
        print(f"\n{CYAN}Monitor detenido.{NC}")
        sys.exit(0)
    signal.signal(signal.SIGINT, _exit)

    print(f"{CYAN}Iniciando monitor — intervalo {interval}s{NC}")
    time.sleep(0.5)

    while True:
        try:
            with _open() as dev:
                s = query_status(dev)
        except Exception as e:
            print(f"{RED}Error leyendo UPS: {e}{NC}")
            time.sleep(interval)
            continue

        if not s:
            time.sleep(interval)
            continue

        draw(s)

        on_batt = s["on_battery"]
        if prev_on_batt is not None and on_batt != prev_on_batt:
            if on_batt:
                notify("UPS — Se fue la luz", "El UPS cambió a batería. Guardá tu trabajo.")
                log("ALERTA: UPS en batería")
            else:
                notify("UPS — Luz restaurada", "El UPS volvió a la red eléctrica.")
                log("OK: UPS en línea")

        if on_batt and s["batt_low"]:
            notify("UPS — Batería crítica", "Batería muy baja. Apagá el equipo ahora.")
            log("CRITICO: Batería baja")

        prev_on_batt = on_batt
        time.sleep(interval)

def cmd_raw():
    with _open() as dev:
        for name, cmd in [("QS", b"QS\r"), ("F", b"F\r")]:
            _send(dev, cmd)
            time.sleep(0.2)
            resp = _recv(dev)
            print(f"{name}: {resp.decode('ascii', errors='replace')}")

# ── Main ──────────────────────────────────────────────────────────────────────
def usage():
    print(f"""
{BOLD}Monitor UPS — Lyonn CTB-800V{NC}

  python3 monitor-ups.py status    Estado actual (una vez)
  python3 monitor-ups.py monitor   Tiempo real (se actualiza cada 5s)
  python3 monitor-ups.py raw       Datos crudos del UPS
""")

if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    match cmd:
        case "status":  cmd_status()
        case "monitor": cmd_monitor()
        case "raw":     cmd_raw()
        case _:         usage()
