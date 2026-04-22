# UPS Monitor — Lyonn CTB-800V (macOS)

Monitor de barra de menú y CLI para el UPS Lyonn CTB-800V en macOS. Funciona via USB HID nativo sin necesidad de drivers ni software adicional del fabricante.

![Estado en barra de menú: ⚡ 100%]

## Compatibilidad

| Dispositivo | VID | PID |
|---|---|---|
| Lyonn CTB-800V | `0x0665` | `0x5161` |

El chip es compatible con la familia Megatec/Voltronic, por lo que puede funcionar con otros UPS del mismo chipset.

## Requisitos

- macOS 12 o superior
- Python 3.10 o superior
- [Homebrew](https://brew.sh)

## Instalación

### 1. Clonar el repositorio

```bash
git clone https://github.com/mauroh/lyonn-ups-monitor.git
cd lyonn-ups-monitor
```

### 2. Instalar dependencias del sistema

```bash
brew install hidapi
```

### 3. Instalar dependencias de Python

```bash
pip3 install hid rumps --break-system-packages
```

> Si usás un entorno virtual, omitir `--break-system-packages`.

### 4. Conectar el UPS

Conectá el cable USB del UPS a la Mac. Verificá que el sistema lo detecta:

```bash
python3 explore-ups.py
```

Deberías ver una respuesta como:

```
UPS encontrado en path: b'DevSrvsID:...'
=== Comando 'QS' ===
  ascii: b'(230.2 232.2 234.6 000 49.8 13.5 --.- 00001001\r'
```

---

## Uso

### App de barra de menú (recomendado)

```bash
python3 ups-menubar.py
```

Aparece un ícono en la barra de menú con el porcentaje de batería. Al hacer click mostrás voltaje de entrada/salida, frecuencia, carga y batería.

| Ícono | Significado |
|---|---|
| ⚡ 100% | En red eléctrica, batería cargada |
| 🔋 85% | Se fue la luz, corriendo con batería |
| ⚠️ 100% | En red pero AVR activo (tensión inestable) |
| ❌ | Falla en el UPS |

Envía **notificaciones del sistema** automáticamente cuando:
- Se va la luz
- Vuelve la luz
- La batería está crítica

### Monitor CLI

```bash
# Estado actual (una vez)
python3 monitor-ups.py status

# Tiempo real (se actualiza cada 5s, Ctrl+C para salir)
python3 monitor-ups.py monitor

# Datos crudos del protocolo
python3 monitor-ups.py raw
```

---

## Inicio automático al encender la Mac

### Instalar el LaunchAgent

Editá el archivo `com.mauroh.ups-monitor.plist` y reemplazá `/Users/mauroh` con tu home directory (`echo $HOME`).

```bash
# Copiar al directorio de LaunchAgents
cp com.mauroh.ups-monitor.plist ~/Library/LaunchAgents/

# Registrar para que arranque con la sesión
launchctl load ~/Library/LaunchAgents/com.mauroh.ups-monitor.plist
```

La app de barra de menú arrancará automáticamente cada vez que inicies sesión. Si se cae, `launchctl` la reinicia sola.

### Detener el inicio automático

```bash
launchctl unload ~/Library/LaunchAgents/com.mauroh.ups-monitor.plist
```

---

## Archivos

| Archivo | Descripción |
|---|---|
| `ups-menubar.py` | App de barra de menú (recomendado) |
| `monitor-ups.py` | Monitor CLI con visualización en tiempo real |
| `explore-ups.py` | Script de exploración del protocolo HID |
| `com.mauroh.ups-monitor.plist` | LaunchAgent para inicio automático |

## Log de eventos

Los eventos (corte de luz, restauración, batería baja) se registran en:

```
~/Library/Logs/ups-monitor.log
```

## Cómo funciona

El UPS usa el **protocolo Megatec QS** sobre USB HID (usage page `0xFF00`). La Mac lo reconoce nativamente sin drivers. El script abre el dispositivo con `hidapi` (IOKit), envía el comando `QS\r` y parsea la respuesta de 47 bytes:

```
(INPUT_V FAULT_V OUTPUT_V LOAD FREQ BATT_V TEMP STATUS\r
```

No requiere `sudo` ni configuración adicional en macOS.
