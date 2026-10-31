//! Herdr desktop mod installation and host-local sampling. The terminal plugin
//! and desktop mod share process measurement and metric formatting.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::{collect, config, model::Space, render};

const MAIN: &str = include_str!("../desktop/main.js");
const MODEL: &str = include_str!("../desktop/model.js");
const MANIFEST: &str = include_str!("../desktop/mod.json");
const DEFAULT_CONFIG: &str = "icons = \"text\"\ninterval_seconds = 5\n";
const BINARY: &str = if cfg!(windows) {
    "space-usage.exe"
} else {
    "space-usage"
};

#[derive(Deserialize)]
struct InputSpace {
    id: String,
    label: String,
    roots: Vec<u32>,
}

fn parse_spaces(input: &str) -> crate::Result<Vec<Space>> {
    let input: Vec<InputSpace> = serde_json::from_str(input)?;
    let mut ids = std::collections::HashSet::new();
    input
        .into_iter()
        .map(|space| {
            if space.id.is_empty() || !ids.insert(space.id.clone()) {
                return Err("desktop spaces need unique, non-empty ids".into());
            }
            let mut roots = space.roots;
            roots.retain(|pid| *pid > 0);
            roots.sort_unstable();
            roots.dedup();
            Ok(Space {
                id: space.id,
                label: space.label,
                roots,
                ..Default::default()
            })
        })
        .collect()
}

pub fn run_json(input: &str) -> crate::Result<()> {
    let mut spaces = parse_spaces(input)?;
    let path = std::env::current_exe()?.with_file_name("config.toml");
    let config = config::load_config_at(&path);
    // A desktop mod has no terminal-side [ui] labels or font settings.
    let labels = config::Labels::default().with_overrides(&config);
    let style = render::RowStyle::from_config(&labels, config.icon_set(), &config);
    // Read whole-device CPU over the same window as the per-space sample.
    // Do not refresh processes here: collect::measure already owns that work.
    let mut system = sysinfo::System::new();
    system.refresh_cpu_usage();
    collect::measure(&mut spaces, 500);
    system.refresh_cpu_usage();
    system.refresh_memory();
    let cpu = f64::from(system.global_cpu_usage());
    let ram_mb = system.used_memory() as f64 / (1024.0 * 1024.0);
    let total_ram_mb = system.total_memory() as f64 / (1024.0 * 1024.0);
    if system.cpus().is_empty() || !cpu.is_finite() || total_ram_mb <= 0.0 || ram_mb > total_ram_mb
    {
        return Err("whole-device CPU or memory counters are unavailable".into());
    }
    let battery = config.battery_reading();
    let disks = config.disk_readings();
    // Keep the existing numeric JSON contract and add the shared display text.
    let mut readings = serde_json::to_value(render::json_spaces(&spaces, battery, &disks))?;
    for (reading, space) in readings.as_array_mut().unwrap().iter_mut().zip(&spaces) {
        reading["text"] = json!(render::usage_row(space.cpu, space.ram_mb, style));
    }
    let totals = render::totals_row(
        spaces.iter().map(|s| s.cpu).sum(),
        spaces.iter().map(|s| s.ram_mb).sum(),
        style,
        battery,
        &disks,
    );
    let system_reading = json!({
        "cpu_percent": cpu.clamp(0.0, 100.0),
        "ram_mb": ram_mb,
        "total_ram_mb": total_ram_mb,
        "ram_percent": 100.0 * ram_mb / total_ram_mb,
        "text": render::totals_row(cpu, ram_mb, style, battery, &disks),
    });
    println!(
        "{}",
        json!({ "spaces": readings, "totals": totals, "system": system_reading,
            "interval_seconds": config.interval_seconds })
    );
    Ok(())
}

pub fn install_arg(args: &[String]) -> crate::Result<PathBuf> {
    let index = args
        .iter()
        .position(|arg| arg == "--install-desktop")
        .unwrap();
    if let Some(path) = args.get(index + 1) {
        if path.starts_with("--") {
            return Err("--install-desktop expects an app home directory".into());
        }
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = config::non_empty_env("HERDR_APP_HOME") {
        return Ok(PathBuf::from(path));
    }
    // appHome() uses ~/.config/herdr-app, including on Windows. Do not use
    // the terminal app's APPDATA/XDG configuration path here.
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("set HERDR_APP_HOME or pass an app home directory")?;
    Ok(PathBuf::from(home).join(".config").join("herdr-app"))
}

pub fn install(app_home: &Path) -> crate::Result<PathBuf> {
    let dir = app_home.join("mods").join("space-usage");
    std::fs::create_dir_all(&dir)?;
    // Copy the executable as well as the mod, so moving a checkout or cleaning
    // target/ cannot break the installed panel. Publish main.js last.
    let executable = std::env::current_exe()?;
    replace_file(
        &dir.join(BINARY),
        &std::fs::read(&executable)?,
        Some(std::fs::metadata(&executable)?.permissions()),
    )?;
    replace_file(&dir.join("model.js"), MODEL.as_bytes(), None)?;
    replace_file(&dir.join("mod.json"), MANIFEST.as_bytes(), None)?;
    let config_path = dir.join("config.toml");
    // The user's settings survive upgrades.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(DEFAULT_CONFIG.as_bytes())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    replace_file(
        &dir.join("main.js"),
        MAIN.replace("@SPACE_USAGE_BINARY@", BINARY).as_bytes(),
        None,
    )?;
    Ok(dir)
}

fn replace_file(
    path: &Path,
    contents: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> crate::Result<()> {
    let temporary = path.with_extension(format!("install-{}.tmp", std::process::id()));
    let result = (|| {
        std::fs::write(&temporary, contents)?;
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&temporary, permissions)?;
        }
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_input_preserves_spaces_and_deduplicates_process_roots() {
        let spaces = parse_spaces(r#"[{"id":"s1","label":"My project","roots":[42,0,42,7]},{"id":"s2","label":"Empty","roots":[]}]"#).unwrap();
        assert_eq!(spaces[0].roots, [7, 42]);
        assert_eq!(spaces[0].label, "My project");
        assert!(spaces[1].roots.is_empty());
    }

    #[test]
    fn invalid_or_ambiguous_desktop_input_is_rejected() {
        for input in [
            "{}",
            r#"[{"id":"s","label":"x","roots":[-1]}]"#,
            r#"[{"id":"s","label":"x","roots":[]},{"id":"s","label":"y","roots":[]}]"#,
        ] {
            assert!(parse_spaces(input).is_err(), "{input}");
        }
    }
}
