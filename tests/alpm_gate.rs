#[allow(dead_code)]
#[path = "support/http_server.rs"]
mod http_server;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

use http_server::{HttpServer, ResponseMode, ServerConfig};
use tempfile::{tempdir, Builder};

struct AlpmGateDirectory {
    directory: Option<tempfile::TempDir>,
    original_owner: String,
    transferred: bool,
}

impl AlpmGateDirectory {
    fn new(directory: tempfile::TempDir) -> Self {
        Self {
            directory: Some(directory),
            original_owner: format!("{}:{}", uid_for(None), current_gid()),
            transferred: false,
        }
    }

    fn path(&self) -> &std::path::Path {
        self.directory
            .as_ref()
            .expect("gate directory is open")
            .path()
    }

    fn transfer_to_alpm(&mut self) -> Result<(), String> {
        self.transferred = true;
        sudo_chown("alpm:alpm", self.path())
    }

    fn reclaim(&mut self) -> Result<(), String> {
        if self.transferred {
            sudo_chown(&self.original_owner, self.path())?;
            self.transferred = false;
        }
        Ok(())
    }

    fn close(mut self) -> Result<(), String> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), String> {
        let Some(directory) = self.directory.take() else {
            return Ok(());
        };
        let path = directory.path().to_owned();

        if self.transferred {
            if let Err(reclaim_error) = sudo_chown(&self.original_owner, &path) {
                return match sudo_remove_directory(&path) {
                    Ok(()) => Ok(()),
                    Err(cleanup_error) => Err(format!(
                        "failed to reclaim gate directory ({reclaim_error}); privileged cleanup also failed ({cleanup_error})"
                    )),
                };
            }
            self.transferred = false;
        }

        match directory.close() {
            Ok(()) => Ok(()),
            Err(close_error) => sudo_remove_directory(&path).map_err(|cleanup_error| {
                format!(
                    "failed to remove reclaimed gate directory ({close_error}); privileged cleanup also failed ({cleanup_error})"
                )
            }),
        }
    }
}

impl Drop for AlpmGateDirectory {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("alpm gate cleanup failed: {error}");
        }
    }
}

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

fn sudo_chown(owner: &str, directory: &std::path::Path) -> Result<(), String> {
    let status = Command::new("sudo")
        .args(["-n", "chown", "--", owner])
        .arg(directory)
        .status()
        .map_err(|error| format!("could not run sudo chown: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("sudo chown exited with {status}"))
}

fn sudo_remove_directory(directory: &std::path::Path) -> Result<(), String> {
    let status = Command::new("sudo")
        .args(["-n", "rm", "-rf", "--"])
        .arg(directory)
        .status()
        .map_err(|error| format!("could not run sudo rm: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("sudo rm exited with {status}"))
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
    let mut directory = AlpmGateDirectory::new(tempdir().expect("create alpm gate directory"));
    let binary = directory.path().join("RustyPac");
    fs::copy(release_binary(), &binary).expect("stage release RustyPac for alpm");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
        .expect("make staged release RustyPac executable by alpm");
    let output = directory.path().join("package.pkg.tar.zst.part");
    assert!(output.is_absolute());
    directory
        .transfer_to_alpm()
        .expect("transfer gate directory to alpm");

    let status = Command::new("sudo")
        .args(["-n", "-u", "alpm", "--"])
        .arg(&binary)
        .arg(server.url("/package.pkg.tar.zst"))
        .arg(&output)
        .status()
        .expect("invoke release RustyPac as alpm");

    directory
        .reclaim()
        .expect("reclaim gate directory from alpm");
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

#[test]
#[ignore = "requires the alpm user and passwordless sudo"]
fn ownership_transfer_is_recovered_after_panic() {
    let directory = Builder::new()
        .prefix("rustypac-alpm-cleanup-")
        .tempdir()
        .expect("create cleanup regression directory");
    let path = directory.path().to_owned();
    fs::write(path.join("owned-by-test-user"), b"cleanup regression")
        .expect("create cleanup regression marker");
    let original_owner = format!("{}:{}", uid_for(None), current_gid());

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut directory = AlpmGateDirectory::new(directory);
        directory
            .transfer_to_alpm()
            .expect("transfer regression directory to alpm");
        panic!("injected failure after ownership transfer");
    }));

    let remained_after_unwind = path.exists();
    if remained_after_unwind {
        sudo_chown(&original_owner, &path).expect("rescue cleanup regression ownership");
        fs::remove_dir_all(&path).expect("clean RED regression artifact");
    }

    assert!(panic.is_err(), "injected failure must unwind");
    assert!(
        !remained_after_unwind,
        "ownership-transferred directory survived unwinding"
    );
}
