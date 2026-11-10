#[allow(dead_code)]
#[path = "support/http_server.rs"]
mod http_server;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use http_server::{HttpServer, ResponseMode, ServerConfig};
use tempfile::tempdir;

fn transfer(home: &Path, config: Option<&Path>, server: &HttpServer) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_RustyPac"));
    command.env("HOME", home).env_remove("RUSTYPAC_CONFIG");
    if let Some(config) = config {
        command.env("RUSTYPAC_CONFIG", config);
    }
    command
        .arg(server.url("/file"))
        .arg(home.join("file.part"))
        .output()
        .unwrap()
}

fn server(size: usize) -> HttpServer {
    HttpServer::spawn(ServerConfig::new(
        "/file",
        vec![42; size],
        ResponseMode::Ranges,
    ))
}

#[test]
fn missing_config_is_generated_under_home_and_used() {
    let home = tempdir().unwrap();
    let result = transfer(home.path(), None, &server(4096));
    assert!(result.status.success(), "{:?}", result);
    let yaml = fs::read_to_string(home.path().join(".config/RustyPac/config.yaml")).unwrap();
    assert!(yaml.contains("resume: true"));
    assert!(yaml.contains("connections_per_file:"));
}

#[test]
fn valid_partial_config_is_read_without_rewriting_and_limits_connections() {
    let home = tempdir().unwrap();
    let path = home.path().join("custom.yaml");
    let yaml = "# Keep my comment\nconnections_per_file: 1\nmin_split_size: 1\n";
    fs::write(&path, yaml).unwrap();
    let server = server(11 * 1024 * 1024);
    let result = transfer(home.path(), Some(&path), &server);
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(fs::read_to_string(path).unwrap(), yaml);
    assert!(
        server
            .requests()
            .iter()
            .filter_map(|r| r.header("range"))
            .all(|r| r.starts_with("bytes=0-")),
        "single connection must not request nonzero segments"
    );
    assert_eq!(
        fs::read(home.path().join("file.part")).unwrap(),
        vec![42; 11 * 1024 * 1024]
    );
}

#[test]
fn server_connection_limit_caps_file_splitting() {
    let home = tempdir().unwrap();
    let path = home.path().join("custom.yaml");
    fs::write(
        &path,
        "connections_per_file: 8\nconnections_per_server: 1\nmin_split_size: 1\n",
    )
    .unwrap();
    let server = server(11 * 1024 * 1024);
    let result = transfer(home.path(), Some(&path), &server);
    assert!(result.status.success(), "{:?}", result);
    assert!(server
        .requests()
        .iter()
        .filter_map(|r| r.header("range"))
        .all(|r| r.starts_with("bytes=0-")));
}

#[test]
fn malformed_or_invalid_settings_are_replaced_with_defaults_and_warn() {
    for yaml in [
        "resume: [broken",
        "connections_per_file: 0\n",
        "read_timeout_secs: 0\n",
        "resume: nonsense\n",
        "unknown_typo: 4\n",
    ] {
        let home = tempdir().unwrap();
        let path = home.path().join("custom.yaml");
        fs::write(&path, yaml).unwrap();
        let result = transfer(home.path(), Some(&path), &server(4096));
        assert!(result.status.success(), "{:?}", result);
        let replacement = fs::read_to_string(&path).unwrap();
        assert_ne!(replacement, yaml);
        assert!(replacement.contains("resume: true"));
        assert!(String::from_utf8_lossy(&result.stderr).contains("default"));
    }
}

#[test]
fn smaller_split_threshold_enables_segments_for_small_files() {
    let home = tempdir().unwrap();
    let path = home.path().join("custom.yaml");
    fs::write(
        &path,
        "min_split_size: 1048576\npiece_size: 262144\nconnections_per_file: 3\n",
    )
    .unwrap();
    let server = server(3 * 1024 * 1024);
    let result = transfer(home.path(), Some(&path), &server);
    assert!(result.status.success(), "{result:?}");
    assert!(
        server
            .requests()
            .iter()
            .filter_map(|r| r.header("range"))
            .any(|r| !r.starts_with("bytes=0-")),
        "configured splitting must request nonzero segments below the old 10 MiB threshold"
    );
    assert_eq!(
        fs::read(home.path().join("file.part")).unwrap(),
        vec![42; 3 * 1024 * 1024]
    );
}

#[test]
fn disable_is_not_blocked_by_an_unreadable_download_config() {
    let home = tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_RustyPac"))
        .env("RUSTYPAC_CONFIG", home.path())
        .arg("--disable")
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&result.stderr).contains("failed to load configuration"),
        "must allow disabling integration even when download settings are unreadable"
    );
}

#[test]
fn config_io_error_fails_without_starting_a_download() {
    let home = tempdir().unwrap();
    let server = server(4096);
    let result = transfer(home.path(), Some(home.path()), &server);
    assert!(!result.status.success());
    assert!(server.requests().is_empty());
}
