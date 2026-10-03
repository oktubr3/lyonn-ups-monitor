//! Notificaciones de macOS y log de eventos en texto.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use chrono::Local;

pub fn log_path() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join("Library/Logs/ups-monitor.log")
}

pub fn log(msg: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        writeln!(f, "{} {msg}", Local::now().format("%Y-%m-%d %H:%M:%S")).ok();
    }
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Muestra una notificación del sistema sin bloquear al llamador.
pub fn notify(title: &str, message: &str) {
    let script = format!(
        "display notification {} with title {} sound name \"Basso\"",
        applescript_string(message),
        applescript_string(title)
    );
    let child = Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        std::thread::spawn(move || child.wait().ok());
    }
}

#[cfg(test)]
mod tests {
    use super::applescript_string;

    #[test]
    fn escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
