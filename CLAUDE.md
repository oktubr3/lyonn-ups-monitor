# ups-monitor

Monitor de red eléctrica y UPS (Lyonn CTB-800V) para macOS, en Rust. Ver `README.md`.

## Verificación

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Para probar la interfaz sin tocar el historial real: `UPS_MONITOR_DATA_DIR=<carpeta> ups-monitor --show`. El UPS admite un solo proceso leyendo: antes hay que salir de la app instalada (`launchctl bootout gui/$(id -u)/com.mauroh.ups-monitor`).

## Sources of version

Valores que se repiten en más de un archivo; al cambiar uno, actualizar todos en el mismo commit.

- **Versión**: solo `Cargo.toml` (`Cargo.lock` se regenera).
- **Texto de uso y opciones**: `src/cli.rs` (`USAGE`), `src/gui.rs` (`Options::parse`) y README "Uso".
- **Bandas de tensión (±5 %, ±8 %)**: `src/model.rs` (`TOL_GOOD`, `TOL_NORM`), README "Bandas de tensión" y la leyenda del gráfico de tensión en `src/gui.rs`.
- **Rutas de datos y logs**: `src/store.rs` (`data_dir`), `src/notify.rs` (`log_path`), `com.mauroh.ups-monitor.plist` y README "Datos".
- **Retención y resolución del historial** (7 días de muestras, cuadro de 14 días × 10 min): `src/store.rs`, `src/gui.rs` y README.
