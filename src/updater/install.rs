//! Native updater handoff and same-filesystem replacement with rollback.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::download::PreparedUpdate;

pub(crate) const HELPER_FLAG: &str = "--velotype-update-helper";
pub(crate) const FINISH_FLAG: &str = "--velotype-update-finished";

/// Canonical destination, with macOS bundles replaced as one unit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct InstallTarget {
    pub path: PathBuf,
    pub executable: PathBuf,
    pub bundle: bool,
}

impl InstallTarget {
    pub fn current() -> Result<Self> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let bundle_path = if cfg!(target_os = "macos") {
            executable
                .ancestors()
                .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        } else {
            None
        };
        let path = bundle_path.unwrap_or(&executable).to_path_buf();
        let bundle = bundle_path.is_some();
        Ok(Self {
            path,
            executable,
            bundle,
        })
    }

    /// Probes the parent directory without modifying the installed application.
    pub fn check_writable(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("installation has no parent directory")?;
        tempfile::Builder::new()
            .prefix(".velotype-update-probe-")
            .tempfile_in(parent)
            .with_context(|| {
                format!(
                    "installation directory is not writable: {}",
                    parent.display()
                )
            })?;
        Ok(())
    }
}

/// Data-only job; paths and arguments never enter a shell command string.
#[derive(Serialize, Deserialize)]
struct InstallJob {
    token: String,
    parent_pid: u32,
    target: InstallTarget,
    payload: PathBuf,
    old_digest: String,
    restart_files: Vec<PathBuf>,
    working_directory: PathBuf,
}

/// Holds the parent's pipe open until shutdown; EOF releases the helper to install.
pub(super) struct Handoff {
    child: Option<Child>,
    pipe: Option<ChildStdin>,
    authorized: bool,
}

impl Handoff {
    pub fn authorize(&mut self) -> Result<()> {
        let pipe = self
            .pipe
            .as_mut()
            .context("update helper has no input pipe")?;
        pipe.write_all(b"APPLY\n")?;
        pipe.flush()?;
        self.authorized = true;
        Ok(())
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        if !self.authorized {
            if let Some(mut pipe) = self.pipe.take() {
                let _ = pipe.write_all(b"ABORT\n");
            }
            if let Some(mut child) = self.child.take() {
                std::thread::spawn(move || {
                    for _ in 0..750 {
                        if child.try_wait().ok().flatten().is_some() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                });
            }
        }
    }
}

fn digest_file(path: &Path) -> Result<String> {
    let mut reader = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(super::hex_digest(hasher.finalize()))
}

fn hidden_command(path: &Path) -> Command {
    let command = Command::new(path);
    #[cfg(windows)]
    let command = {
        let mut command = command;
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    };
    command
}

/// Prepares a private helper and requires a successful readiness handshake.
pub(super) fn launch(prepared: &PreparedUpdate, restart_files: Vec<PathBuf>) -> Result<Handoff> {
    prepared.target.check_writable()?;
    let directory = prepared.directory.path();
    let helper = directory.join(if cfg!(windows) {
        "helper.exe"
    } else {
        "helper"
    });
    fs::copy(std::env::current_exe()?, &helper)?;
    let token = uuid::Uuid::new_v4().to_string();
    let job = InstallJob {
        token: token.clone(),
        parent_pid: std::process::id(),
        target: prepared.target.clone(),
        payload: prepared.payload.strip_prefix(directory)?.to_path_buf(),
        old_digest: digest_file(&helper)?,
        restart_files,
        working_directory: std::env::current_dir()?,
    };
    fs::write(directory.join("job.json"), serde_json::to_vec(&job)?)?;
    let mut child = hidden_command(&helper)
        .arg(HELPER_FLAG)
        .arg(directory)
        .arg(token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to launch update helper")?;
    let stdout = child
        .stdout
        .take()
        .context("update helper has no output pipe")?;
    let pipe = child.stdin.take();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    let handoff = Handoff {
        child: Some(child),
        pipe,
        authorized: false,
    };
    let response = receiver
        .recv_timeout(Duration::from_secs(15))
        .context("update helper readiness timed out")??;
    ensure!(
        response.trim() == "READY",
        "update helper rejected the job: {}",
        response.trim()
    );
    Ok(handoff)
}

/// Accepts only our private temporary job directory and its matching secret token.
fn read_job(directory: &Path, token: &str) -> Result<InstallJob> {
    let canonical = directory.canonicalize()?;
    ensure!(
        canonical.parent() == Some(std::env::temp_dir().canonicalize()?.as_path())
            && canonical
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("velotype-update-")),
        "invalid update job directory"
    );
    let file = File::open(canonical.join("job.json"))?;
    let job: InstallJob = serde_json::from_reader(file.take(1024 * 1024))?;
    ensure!(
        job.token == token && uuid::Uuid::parse_str(token).is_ok(),
        "invalid update job token"
    );
    ensure!(
        job.payload
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
            && job.payload.starts_with("payload"),
        "invalid staged payload path"
    );
    Ok(job)
}

/// Copies a validated payload while rejecting filesystem links and special files.
fn copy_payload(from: &Path, to: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "update payload contains a symbolic link"
    );
    if metadata.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_payload(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        ensure!(metadata.is_file(), "update payload contains a special file");
        fs::copy(from, to)?;
        fs::set_permissions(to, metadata.permissions())?;
    }
    Ok(())
}

/// Installs with a sibling backup; a failed swap or launch restores the original.
fn replace_and_launch(
    target: &Path,
    staged: &Path,
    backup: &Path,
    mut launch: impl FnMut() -> Result<()>,
) -> Result<()> {
    fs::rename(target, backup).context("failed to back up installed application")?;
    if let Err(error) = fs::rename(staged, target) {
        fs::rename(backup, target).context("update failed and restoring the backup failed")?;
        return Err(error).context("failed to install staged update; original restored");
    }
    if let Err(error) = launch() {
        fs::rename(target, staged).context("failed to move unsuccessful update aside")?;
        fs::rename(backup, target).context("failed to restore original application")?;
        return Err(error).context("failed to restart update; original restored");
    }
    Ok(())
}

fn restart(job: &InstallJob, directory: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if job.target.bundle {
        let status = Command::new("/usr/bin/open")
            .arg("-n")
            .arg(&job.target.path)
            .arg("--args")
            .arg(FINISH_FLAG)
            .arg(directory)
            .arg(&job.token)
            .arg("--")
            .args(&job.restart_files)
            .status()?;
        ensure!(
            status.success(),
            "macOS could not reopen the application bundle"
        );
        return Ok(());
    }
    hidden_command(&job.target.executable)
        .arg(FINISH_FLAG)
        .arg(directory)
        .arg(&job.token)
        .arg("--")
        .args(&job.restart_files)
        .current_dir(if job.working_directory.is_dir() {
            job.working_directory.as_path()
        } else {
            job.target
                .path
                .parent()
                .context("missing restart directory")?
        })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Holds a real process handle so Windows cannot confuse a reused PID with our parent.
#[cfg(windows)]
struct ParentProcess(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ParentProcess {
    fn open(pid: u32) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        // The handle is owned by this guard and needs only synchronization access.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        ensure!(
            !handle.is_null(),
            "cannot wait for update parent: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(handle))
    }

    fn wait(&self) -> Result<()> {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        // OpenProcess returned a live handle retained until this wait completes.
        ensure!(
            unsafe { WaitForSingleObject(self.0, 30_000) } == WAIT_OBJECT_0,
            "update parent did not exit in time"
        );
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ParentProcess {
    fn drop(&mut self) {
        // This guard exclusively owns the handle returned by OpenProcess.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Runs before GPUI initialization in a copied executable, never in the editor process.
pub(crate) fn run_helper(directory: &Path, token: &str) -> Result<()> {
    let job = read_job(directory, token)?;
    #[cfg(windows)]
    let parent_process = ParentProcess::open(job.parent_pid)?;
    let helper = std::env::current_exe()?.canonicalize()?;
    ensure!(
        helper.parent() == Some(directory.canonicalize()?.as_path()),
        "helper is outside its job directory"
    );
    ensure!(
        digest_file(&helper)? == job.old_digest
            && digest_file(&job.target.executable)? == job.old_digest,
        "installed application changed during update"
    );
    ensure!(
        job.target.path.canonicalize()? == job.target.path,
        "installation path is not canonical"
    );
    if job.target.bundle {
        ensure!(
            job.target.executable == job.target.path.join("Contents/MacOS/velotype"),
            "invalid bundle destination"
        );
    } else {
        ensure!(
            job.target.path == job.target.executable,
            "invalid executable destination"
        );
    }
    job.target.check_writable()?;
    let parent = job
        .target
        .path
        .parent()
        .context("missing installation directory")?;
    let lock_name = format!(
        "velotype-install-{}.lock",
        super::hex_digest(Sha256::digest(
            job.target.path.as_os_str().as_encoded_bytes()
        ))
    );
    let lock = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(std::env::temp_dir().join(lock_name))?;
    lock.try_lock()
        .context("another updater is installing this application")?;
    let staging = tempfile::Builder::new()
        .prefix(".velotype-update-stage-")
        .tempdir_in(parent)?;
    let staged = staging.path().join("new");
    copy_payload(&directory.join(&job.payload), &staged)?;
    let backup = staging.path().join("backup");
    println!("READY");
    std::io::stdout().flush()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let mut input = BufReader::new(std::io::stdin());
            let mut command = String::new();
            input.read_line(&mut command)?;
            ensure!(command == "APPLY\n", "installation was not authorized");
            // EOF follows parent shutdown; an inherited pipe cannot be confused by PID reuse.
            let mut remainder = Vec::new();
            input.take(1024).read_to_end(&mut remainder)?;
            ensure!(remainder.is_empty(), "unexpected updater input");
            Ok(())
        })();
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(Duration::from_secs(120))
        .context("parent shutdown timed out")??;
    #[cfg(windows)]
    parent_process.wait()?;
    let result = (|| -> Result<()> {
        let lease = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(".lease"))?;
        lease.try_lock().context("update job is still in use")?;
        ensure!(
            digest_file(&job.target.executable)? == job.old_digest,
            "installed application changed before replacement"
        );
        replace_and_launch(&job.target.path, &staged, &backup, || {
            fs::write(directory.join("complete"), b"done")?;
            restart(&job, directory)
        })
    })();
    if let Err(error) = &result {
        // A full disk must not prevent attempting to reopen the restored application.
        let _ = fs::write(directory.join("error.txt"), format!("{error:#}"));
        let _ = fs::write(directory.join("complete"), b"done");
        if !backup.exists() {
            let _ = restart(&job, directory);
        }
    }
    if result.is_err() && backup.exists() {
        // Keep the only recoverable copy if rollback itself failed.
        let _ = staging.keep();
    }
    result
}

/// Reads a helper receipt and cleans only that authenticated job after helper exit.
pub(crate) fn finish(directory: &Path, token: &str) -> Result<Option<String>> {
    read_job(directory, token)?;
    let directory = directory.to_path_buf();
    ensure!(
        directory.join("complete").is_file(),
        "update has no completion receipt"
    );
    let error = fs::read_to_string(directory.join("error.txt")).ok();
    std::thread::spawn(move || {
        for _ in 0..100 {
            if !directory.join("complete").is_file() {
                return;
            }
            if fs::remove_dir_all(&directory).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    Ok(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_swaps_special_character_paths_and_rolls_back_failed_launch() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Velotype ' 中文 & test.exe");
        let staged = dir.path().join("new");
        let backup = dir.path().join("backup");
        fs::write(&target, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        assert!(
            replace_and_launch(&target, &staged, &backup, || anyhow::bail!("launch failed"))
                .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"old");
        replace_and_launch(&target, &staged, &backup, || Ok(())).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(&backup).unwrap(), b"old");
    }

    #[test]
    fn updater_restores_original_if_staged_payload_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("velotype");
        fs::write(&target, b"old").unwrap();
        assert!(
            replace_and_launch(
                &target,
                &dir.path().join("missing"),
                &dir.path().join("backup"),
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn updater_bundle_swap_preserves_resources_and_restores_on_error() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("Velotype.app");
        let staged = directory.path().join("new.app");
        let backup = directory.path().join("backup.app");
        fs::create_dir_all(target.join("Contents/Resources")).unwrap();
        fs::write(target.join("Contents/Resources/old"), b"old").unwrap();
        fs::create_dir_all(staged.join("Contents/Resources")).unwrap();
        fs::write(staged.join("Contents/Resources/new"), b"new").unwrap();
        assert!(
            replace_and_launch(&target, &staged, &backup, || anyhow::bail!(
                "launch rejected"
            ))
            .is_err()
        );
        assert!(target.join("Contents/Resources/old").is_file());
        assert!(!target.join("Contents/Resources/new").exists());
        replace_and_launch(&target, &staged, &backup, || Ok(())).unwrap();
        assert!(target.join("Contents/Resources/new").is_file());
        assert!(!target.join("Contents/Resources/old").exists());
    }
}
