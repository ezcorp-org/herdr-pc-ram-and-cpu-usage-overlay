use std::path::{Path, PathBuf};
use std::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_space-usage");

struct AppHome(PathBuf);

impl AppHome {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("space-usage-desktop-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for AppHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn install(home: &Path, explicit: bool) {
    let mut command = Command::new(BINARY);
    command.arg("--install-desktop");
    if explicit {
        command.arg(home);
    } else {
        command.env("HERDR_APP_HOME", home);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn install_upgrade_and_sample_without_a_terminal_app_server() {
    let home = AppHome::new();
    let app_home = home.0.join("app home with spaces");
    install(&app_home, false);
    let dir = app_home.join("mods/space-usage");
    for file in ["main.js", "model.js", "mod.json", "config.toml"] {
        assert!(dir.join(file).is_file(), "missing {file}");
    }
    let main = std::fs::read_to_string(dir.join("main.js")).unwrap();
    assert!(!main.contains("@SPACE_USAGE_BINARY@"));
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mod.json")).unwrap()).unwrap();
    assert_eq!(manifest["place"], "bottom");

    let settings = "icons = \"text\"\ncpu_label = \"LOAD\"\nram_display = \"gb\"\nbattery = false\ndisk = false\ninterval_seconds = 7\n";
    std::fs::write(dir.join("config.toml"), settings).unwrap();
    install(&app_home, true);
    assert_eq!(
        std::fs::read_to_string(dir.join("config.toml")).unwrap(),
        settings
    );
    let executable = dir.join(if cfg!(windows) {
        "space-usage.exe"
    } else {
        "space-usage"
    });
    let input = serde_json::json!([
        {"id":"live","label":"Test process","roots":[std::process::id()]},
        {"id":"empty","label":"Empty space","roots":[]}
    ]);
    let output = Command::new(&executable)
        .args(["--desktop-json", &input.to_string()])
        .env("HERDR_SOCKET_PATH", home.0.join("no-terminal-server.sock"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let system = &result["system"];
    assert!(system["total_ram_mb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(system["ram_mb"].as_f64().unwrap_or(0.0) > 0.0);
    assert!(system["ram_mb"].as_f64().unwrap() <= system["total_ram_mb"].as_f64().unwrap());
    assert!((0.0..=100.0).contains(&system["cpu_percent"].as_f64().unwrap()));
    assert!(system["text"].as_str().unwrap().contains("LOAD"));
    assert_eq!(result["interval_seconds"], 7);
    let live = &result["spaces"][0];
    assert_eq!(live["workspace_id"], "live");
    assert!(live["ram_mb"].as_f64().unwrap() > 0.0);
    assert!(live["processes"].as_u64().unwrap() > 0);
    assert!(live["cpu_percent"].as_f64().unwrap() >= 0.0);
    assert!(live["text"].as_str().unwrap().contains("LOAD"));
    assert_eq!(result["spaces"][1]["ram_mb"], 0.0);
    assert!(result["totals"].as_str().unwrap().contains("LOAD"));

    // Whole-device readings must work even when no Herdr space has a process.
    let empty = Command::new(&executable)
        .args(["--desktop-json", "[]"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    let empty: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert!(empty["spaces"].as_array().unwrap().is_empty());
    assert!(empty["system"]["ram_mb"].as_f64().unwrap() > 0.0);
    assert_eq!(empty["system"]["total_ram_mb"], system["total_ram_mb"]);

    let invalid = Command::new(executable)
        .args(["--desktop-json", "not-json"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    // Desktop installation creates no terminal config and edits no layout.
    assert!(!app_home.join("config.toml").exists());
    assert!(!app_home.join("layouts").exists());
}
