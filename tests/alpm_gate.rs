#[allow(dead_code)]
#[path = "support/http_server.rs"]
mod http_server;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

use http_server::{HttpServer, ResponseMode, ServerConfig};
use tempfile::tempdir;

fn body(size: usize) -> Vec<u8> {
    (0..size).map(|index| (index % 251) as u8).collect()
}

fn release_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("release")
        .join("RustyPac")
}

fn uid_for(user: Option<&str>) -> u32 {
    let mut command = Command::new("id");
    command.arg("-u");
    if let Some(user) = user {
        command.arg(user);
    }
    let output = command.output().expect("query uid");
    assert!(output.status.success(), "requested user must exist");
    String::from_utf8(output.stdout)
        .expect("uid is UTF-8")
        .trim()
        .parse()
        .expect("uid is numeric")
}

fn current_gid() -> u32 {
    let output = Command::new("id")
        .arg("-g")
        .output()
        .expect("query current gid");
    assert!(output.status.success(), "current group must exist");
    String::from_utf8(output.stdout)
        .expect("gid is UTF-8")
        .trim()
        .parse()
        .expect("gid is numeric")
}

fn chown_directory(owner: &str, directory: &std::path::Path) {
    let status = Command::new("sudo")
        .args(["-n", "chown", "--", owner])
        .arg(directory)
        .status()
        .expect("change gate directory ownership");
    assert!(status.success(), "change gate directory ownership");
}

#[test]
#[ignore = "requires the alpm user, passwordless sudo, and a prebuilt release binary"]
fn release_binary_writes_exact_absolute_part_output_as_alpm() {
    let expected = body(1024 * 1024 + 17);
    let server = HttpServer::spawn(ServerConfig::new(
        "/package.pkg.tar.zst",
        expected.clone(),
        ResponseMode::Ranges,
    ));
    let directory = tempdir().expect("create alpm gate directory");
    let binary = directory.path().join("RustyPac");
    fs::copy(release_binary(), &binary).expect("stage release RustyPac for alpm");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
        .expect("make staged release RustyPac executable by alpm");
    let output = directory.path().join("package.pkg.tar.zst.part");
    assert!(output.is_absolute());
    chown_directory("alpm:alpm", directory.path());

    let status = Command::new("sudo")
        .args(["-n", "-u", "alpm", "--"])
        .arg(&binary)
        .arg(server.url("/package.pkg.tar.zst"))
        .arg(&output)
        .status()
        .expect("invoke release RustyPac as alpm");

    let original_owner = format!("{}:{}", uid_for(None), current_gid());
    chown_directory(&original_owner, directory.path());
    let actual = fs::read(&output).expect("read alpm output");
    let output_uid = fs::metadata(&output).expect("stat alpm output").uid();
    let cleanup = directory.close();

    assert!(status.success(), "release RustyPac must succeed as alpm");
    assert_eq!(actual, expected, "alpm output must match the HTTP body");
    assert_eq!(
        output_uid,
        uid_for(Some("alpm")),
        "output must be owned by alpm"
    );
    cleanup.expect("remove alpm gate directory and files");
}
