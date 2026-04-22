#!/usr/bin/env python3
"""Script de exploración: descubre el protocolo HID del Lyonn CTB-800V."""

import hid
import time

VENDOR_ID  = 0x0665
PRODUCT_ID = 0x5161

COMMANDS = [
    ("QS",   b"QS\r"),
    ("Q1",   b"Q1\r"),
    ("F",    b"F\r"),
    ("I",    b"I\r"),
    ("QMOD", b"QMOD\r"),
]

def send_cmd(dev, raw: bytes):
    padded = raw + b'\x00' * (8 - len(raw))
    dev.write(b'\x00' + padded[:8])  # 0x00 = sin report ID

def read_chunks(dev, chunks=8, timeout_ms=500) -> bytes:
    buf = b""
    for _ in range(chunks):
        data = dev.read(8, timeout_ms)
        if not data:
            break
        buf += bytes(data)
        if buf.endswith(b"\r"):
            break
    return buf

def main():
    devs = hid.enumerate(VENDOR_ID, PRODUCT_ID)
    if not devs:
        print("UPS no encontrado. Verificá el cable USB.")
        return

    print(f"UPS encontrado en path: {devs[0]['path']}")
    print(f"  usage_page = 0x{devs[0]['usage_page']:04X}")
    print()

    with hid.Device(VENDOR_ID, PRODUCT_ID) as dev:
        dev.nonblocking = True

        # Leer datos espontáneos al abrir
        print("=== Datos espontáneos al conectar ===")
        for _ in range(8):
            d = dev.read(64, 100)
            if d:
                print(f"  raw hex : {bytes(d).hex()}")
                print(f"  ascii   : {bytes(d)}")
            time.sleep(0.1)
        print()

        dev.nonblocking = False

        # Probar descriptor
        print("=== Report descriptor ===")
        try:
            desc = dev.get_report_descriptor()
            print(f"  {bytes(desc).hex()}")
        except Exception as e:
            print(f"  error: {e}")
        print()

        # Probar feature report
        print("=== Feature report (ID 0, 9 bytes) ===")
        try:
            fr = dev.get_feature_report(0, 9)
            print(f"  hex  : {bytes(fr).hex()}")
            print(f"  ascii: {bytes(fr)}")
        except Exception as e:
            print(f"  error: {e}")
        print()

        # Probar cada comando
        for name, cmd in COMMANDS:
            print(f"=== Comando {name!r} ===")
            try:
                send_cmd(dev, cmd)
                time.sleep(0.3)
                resp = read_chunks(dev)
                if resp:
                    print(f"  hex  : {resp.hex()}")
                    print(f"  ascii: {resp}")
                else:
                    print("  (sin respuesta)")
            except Exception as e:
                print(f"  error: {e}")
            print()
            time.sleep(0.2)

if __name__ == "__main__":
    main()
