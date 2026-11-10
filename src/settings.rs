use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytehaul::{DownloadSpec, FileAllocation};
use serde::{Deserialize, Serialize};

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
    let path = if let Some(path) = std::env::var_os("RUSTYPAC_CONFIG").filter(|p| !p.is_empty()) {
        PathBuf::from(path)
    } else {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        PathBuf::from(home).join(".config/RustyPac/config.yaml")
    };
    load_path(&path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

fn load_path(path: &Path) -> io::Result<Settings> {
    match fs::read(path) {
        Ok(bytes) => {
            if let Ok(yaml) = std::str::from_utf8(&bytes) {
                if let Ok(settings) = serde_saphyr::from_str::<Settings>(yaml) {
                    if settings.valid() {
                        return Ok(settings);
                    }
                }
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
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(b"# RustyPac download settings. Sizes/speed are bytes and bytes/sec (0 = unlimited).\n# resume: false restarts incomplete files. Connections are capped by the smaller limit.\n# Pacman's XferCommand supplies one file at a time; parallel package files are not supported.\n")?;
    let yaml = serde_saphyr::to_string(&settings).map_err(io::Error::other)?;
    temporary.write_all(yaml.as_bytes())?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(settings)
}
