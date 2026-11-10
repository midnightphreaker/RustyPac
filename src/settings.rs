use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytehaul::{DownloadSpec, FileAllocation};
use serde::{Deserialize, Serialize};

const SYSTEM_CONFIG: &str = "/etc/RustyPac/config.yaml";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub resume: bool,
    pub connections_per_file: u32,
    pub connections_per_server: u32,
    pub min_split_size: u64,
    pub piece_size: u64,
    pub connect_timeout_secs: u64,
    pub read_timeout_secs: u64,
    pub max_retries: u32,
    pub max_download_speed: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resume: true,
            connections_per_file: 4,
            connections_per_server: 4,
            min_split_size: 10 * 1024 * 1024,
            piece_size: 1024 * 1024,
            connect_timeout_secs: 30,
            read_timeout_secs: 60,
            max_retries: 5,
            max_download_speed: 0,
        }
    }
}

impl Settings {
    pub fn spec(&self, url: &str, output: &Path) -> DownloadSpec {
        // Each invocation has one URL/server, so the smaller cap applies.
        DownloadSpec::new(url)
            .output_path(output.to_owned())
            .file_allocation(FileAllocation::None)
            .resume(self.resume)
            .max_connections(self.connections_per_file.min(self.connections_per_server))
            .min_split_size(self.min_split_size)
            .piece_size(self.piece_size)
            .connect_timeout(Duration::from_secs(self.connect_timeout_secs))
            .read_timeout(Duration::from_secs(self.read_timeout_secs))
            .max_retries(self.max_retries)
            .max_download_speed(self.max_download_speed)
    }

    fn valid(&self) -> bool {
        (1..=128).contains(&self.connections_per_file)
            && (1..=128).contains(&self.connections_per_server)
            && self.min_split_size > 0
            && (1..=64 * 1024 * 1024).contains(&self.piece_size)
            && (1..=86400).contains(&self.connect_timeout_secs)
            && (1..=86400).contains(&self.read_timeout_secs)
    }
}

pub fn load() -> io::Result<Settings> {
    if let Some(path) = std::env::var_os("RUSTYPAC_CONFIG").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        return load_path(&path)
            .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())));
    }
    let shared = Path::new(SYSTEM_CONFIG);
    match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => load_implicit(
            &PathBuf::from(home).join(".config/RustyPac/config.yaml"),
            shared,
        ),
        None => Ok(load_shared(shared)),
    }
}

fn load_implicit(home: &Path, shared: &Path) -> io::Result<Settings> {
    // Pacman can retain HOME=/root after dropping to its download account.
    // An implicit private path must not prevent package downloads.
    match load_path(home) {
        Ok(settings) => Ok(settings),
        Err(_) => Ok(load_shared(shared)),
    }
}

fn load_shared(path: &Path) -> Settings {
    if let Some(settings) = fs::read(path).ok().and_then(|bytes| parse_settings(&bytes)) {
        return settings;
    }
    eprintln!(
        "RustyPac: personal configuration unavailable and no valid {}; using built-in defaults",
        path.display()
    );
    Settings::default()
}

fn parse_settings(bytes: &[u8]) -> Option<Settings> {
    let yaml = std::str::from_utf8(bytes).ok()?;
    let settings: Settings = serde_saphyr::from_str(yaml).ok()?;
    settings.valid().then_some(settings)
}

fn write_system_copy(path: &Path, settings: &Settings) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("system configuration has no parent"))?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    write_settings(path, settings, 0o644)
}

pub fn publish_for_pacman(settings: &Settings) -> io::Result<()> {
    write_system_copy(Path::new(SYSTEM_CONFIG), settings)
}

fn load_path(path: &Path) -> io::Result<Settings> {
    match fs::read(path) {
        Ok(bytes) => {
            if let Some(settings) = parse_settings(&bytes) {
                return Ok(settings);
            }
            eprintln!(
                "RustyPac: invalid configuration at {}; replacing with defaults",
                path.display()
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let settings = Settings::default();
    write_settings(path, &settings, 0o600)?;
    Ok(settings)
}

fn write_settings(path: &Path, settings: &Settings, mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(b"# RustyPac download settings. Sizes/speed are bytes and bytes/sec (0 = unlimited).\n# resume: false restarts incomplete files. Connections are capped by the smaller limit.\n# Pacman's XferCommand supplies one file at a time; parallel package files are not supported.\n")?;
    let yaml = serde_saphyr::to_string(settings).map_err(io::Error::other)?;
    temporary.write_all(yaml.as_bytes())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inaccessible_home_uses_shared_settings_without_rewriting_them() {
        let directory = tempfile::tempdir().unwrap();
        let unusable_home = directory.path().join("not-a-directory");
        fs::write(&unusable_home, b"leave alone").unwrap();
        let shared = directory.path().join("system.yaml");
        let yaml = "# shared settings\nresume: false\nconnections_per_file: 7\n";
        fs::write(&shared, yaml).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o444)).unwrap();
        let settings = load_implicit(&unusable_home.join("config.yaml"), &shared).unwrap();
        assert!(!settings.resume);
        assert_eq!(settings.connections_per_file, 7);
        assert_eq!(fs::read_to_string(shared).unwrap(), yaml);
    }

    #[test]
    fn publishing_makes_a_readable_copy_of_selected_settings() {
        let directory = tempfile::tempdir().unwrap();
        let shared = directory.path().join("RustyPac/config.yaml");
        let settings = Settings {
            connections_per_file: 9,
            resume: false,
            ..Settings::default()
        };
        write_system_copy(&shared, &settings).unwrap();
        let actual: Settings =
            serde_saphyr::from_str(&fs::read_to_string(&shared).unwrap()).unwrap();
        assert_eq!(actual.connections_per_file, 9);
        assert!(!actual.resume);
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            fs::metadata(shared.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}
