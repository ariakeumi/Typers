//! Release asset selection, verified streaming downloads, and bounded extraction.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::install::InstallTarget;

const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 1024 * 1024 * 1024;

/// Owns a validated payload until installation explicitly transfers its lifetime.
pub(super) struct PreparedUpdate {
    pub version: String,
    /// Prevents another instance from cleaning this job while it is ready or downloading.
    /// Declared before the directory so Windows releases the handle before cleanup.
    pub _lease: File,
    pub directory: TempDir,
    pub payload: PathBuf,
    pub target: InstallTarget,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

/// Maps the compiled platform to exactly one archive from the release workflow.
pub(super) fn asset_name(version: &str, os: &str, arch: &str) -> Result<String> {
    let version = semver::Version::parse(version)?;
    ensure!(
        version.pre.is_empty(),
        "preview versions cannot be installed automatically"
    );
    ensure!(
        matches!(arch, "x86_64" | "aarch64"),
        "unsupported update architecture: {arch}"
    );
    let extension = match os {
        "windows" | "macos" => "zip",
        "linux" => "tar.gz",
        _ => bail!("unsupported update platform: {os}"),
    };
    Ok(format!("velotype-v{version}-{os}-{arch}.{extension}"))
}

/// Requires an exact stable release, asset URL, size, and SHA-256 digest.
fn select_asset(release: Release, version: &str, os: &str, arch: &str) -> Result<Asset> {
    let name = asset_name(version, os, arch)?;
    let tag = format!("v{version}");
    ensure!(
        release.tag_name == tag && !release.draft && !release.prerelease,
        "the corresponding stable release is not available"
    );
    let mut matching = release
        .assets
        .into_iter()
        .filter(|asset| asset.name == name);
    let asset = matching
        .next()
        .context("release has no matching platform archive")?;
    ensure!(
        matching.next().is_none(),
        "release contains duplicate platform archives"
    );
    ensure!(
        asset.size > 0 && asset.size <= MAX_ARCHIVE_BYTES,
        "invalid update archive size"
    );
    ensure!(
        asset.browser_download_url
            == format!("https://github.com/manyougz/velotype/releases/download/{tag}/{name}"),
        "unexpected release download URL"
    );
    sha256_digest(asset.digest.as_deref())?;
    Ok(asset)
}

fn sha256_digest(digest: Option<&str>) -> Result<&str> {
    let digest = digest
        .and_then(|value| value.strip_prefix("sha256:"))
        .context("release archive has no SHA-256 digest")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "release archive has an invalid SHA-256 digest"
    );
    Ok(digest)
}

pub(super) fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "update download cancelled"
    );
    Ok(())
}

/// Streams through a fixed buffer, rejecting truncation, excess data, and digest mismatches.
fn copy_verified(
    input: &mut impl Read,
    output: &mut impl Write,
    expected_size: u64,
    expected_digest: &str,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancelled(cancelled)?;
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        ensure!(
            size <= expected_size && size <= MAX_ARCHIVE_BYTES,
            "update archive exceeds expected size"
        );
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    ensure!(size == expected_size, "update archive was truncated");
    ensure!(
        super::hex_digest(hasher.finalize()).eq_ignore_ascii_case(expected_digest),
        "update archive SHA-256 verification failed"
    );
    output.flush()?;
    Ok(())
}

/// Fetches only official HTTPS endpoints; asset redirects stay on GitHub-owned hosts.
pub(super) fn prepare(version: &str, cancelled: &AtomicBool) -> Result<PreparedUpdate> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    asset_name(version, os, arch)?;
    let target = InstallTarget::current()?;
    target.check_writable()?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("Velotype/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let host = attempt.url().host_str().unwrap_or_default();
            if attempt.previous().len() < 5
                && attempt.url().scheme() == "https"
                && (host == "github.com" || host.ends_with(".githubusercontent.com"))
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()?;
    check_cancelled(cancelled)?;
    let response = client
        .get(format!(
            "https://api.github.com/repos/manyougz/velotype/releases/tags/v{version}"
        ))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(Duration::from_secs(10))
        .send()?
        .error_for_status()?;
    let mut metadata = Vec::new();
    response.take(2 * 1024 * 1024).read_to_end(&mut metadata)?;
    let asset = select_asset(serde_json::from_slice(&metadata)?, version, os, arch)?;
    check_cancelled(cancelled)?;
    let directory = tempfile::Builder::new()
        .prefix("velotype-update-")
        .tempdir()?;
    let lease = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.path().join(".lease"))?;
    lease.try_lock()?;
    fs::write(directory.path().join(".velotype-update"), b"v1")?;
    let archive = directory.path().join(&asset.name);
    let mut response = client
        .get(&asset.browser_download_url)
        .send()?
        .error_for_status()?;
    let mut file = File::create(&archive)?;
    copy_verified(
        &mut response,
        &mut file,
        asset.size,
        sha256_digest(asset.digest.as_deref())?,
        cancelled,
    )?;
    file.sync_all()?;
    let extracted = directory.path().join("payload");
    fs::create_dir(&extracted)?;
    extract_archive(&archive, &extracted, os, cancelled)?;
    let payload = if os == "macos" {
        if target.bundle {
            extracted.join("Velotype.app")
        } else {
            extracted.join("Velotype.app/Contents/MacOS/velotype")
        }
    } else {
        extracted.join(if os == "windows" {
            "velotype.exe"
        } else {
            "velotype"
        })
    };
    let executable = if target.bundle {
        payload.join("Contents/MacOS/velotype")
    } else {
        payload.clone()
    };
    ensure!(
        executable.is_file() && executable.metadata()?.len() > 0,
        "archive has no executable payload"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    }
    Ok(PreparedUpdate {
        version: version.into(),
        directory,
        _lease: lease,
        payload,
        target,
    })
}

/// Reclaims interrupted jobs only after a day and only when no process holds their lease.
pub(super) fn cleanup_stale() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("velotype-update-")
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let path = entry.path();
        let marker = path.join(".velotype-update");
        if fs::read(&marker).ok().as_deref() != Some(b"v1") {
            continue;
        }
        let stale = marker
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if !stale {
            continue;
        }
        let Ok(lease) = File::options()
            .read(true)
            .write(true)
            .open(path.join(".lease"))
        else {
            continue;
        };
        if lease.try_lock().is_err() {
            continue;
        }
        drop(lease);
        let _ = fs::remove_dir_all(path);
    }
}

/// Normalizes portable archive paths before any filesystem access.
fn safe_archive_path(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.starts_with(['/', '\\']) && !name.contains([':', '\0']),
        "unsafe archive path"
    );
    let mut path = PathBuf::new();
    for part in name.split(['/', '\\']) {
        ensure!(part != "..", "archive path traversal rejected");
        if part.is_empty() || part == "." {
            continue;
        }
        ensure!(
            !part.ends_with(['.', ' ']),
            "ambiguous archive path rejected"
        );
        path.push(part);
    }
    Ok(path)
}

fn wanted_path(path: &Path, os: &str) -> bool {
    match os {
        "macos" => path.starts_with("Velotype.app"),
        "windows" => path == Path::new("velotype.exe"),
        _ => path == Path::new("velotype"),
    }
}

/// Extracts regular payload entries only; archive links never reach the filesystem.
fn extract_archive(archive: &Path, output: &Path, os: &str, cancelled: &AtomicBool) -> Result<()> {
    let mut total = 0u64;
    let mut extract =
        |name: &str, directory: bool, size: u64, input: &mut dyn Read| -> Result<()> {
            check_cancelled(cancelled)?;
            let path = safe_archive_path(name)?;
            total = total.checked_add(size).context("archive size overflow")?;
            ensure!(
                total <= MAX_EXTRACTED_BYTES,
                "extracted update is too large"
            );
            if !wanted_path(&path, os) {
                return Ok(());
            }
            let target = output.join(path);
            if directory {
                fs::create_dir_all(target)?;
            } else {
                fs::create_dir_all(target.parent().context("invalid archive entry")?)?;
                let mut file = File::options().write(true).create_new(true).open(target)?;
                let written = std::io::copy(&mut input.take(size + 1), &mut file)?;
                ensure!(written == size, "invalid extracted entry size");
            }
            Ok(())
        };
    if os == "linux" {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
        for (index, entry) in archive.entries()?.enumerate() {
            ensure!(index < 100_000, "too many archive entries");
            let mut entry = entry?;
            let kind = entry.header().entry_type();
            ensure!(
                kind.is_file() || kind.is_dir(),
                "archive links and special entries rejected"
            );
            let name = entry
                .path()?
                .to_str()
                .context("invalid archive path encoding")?
                .to_string();
            extract(&name, kind.is_dir(), entry.size(), &mut entry)?;
        }
    } else {
        let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
        ensure!(archive.len() < 100_000, "too many archive entries");
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
            ensure!(
                matches!(kind, 0 | 0o100000 | 0o040000),
                "archive links and special entries rejected"
            );
            extract(
                &entry.name()?.into_owned(),
                entry.is_dir(),
                entry.size(),
                &mut entry,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_all_six_platform_archives() {
        for os in ["windows", "linux", "macos"] {
            for arch in ["x86_64", "aarch64"] {
                let name = asset_name("1.2.3", os, arch).unwrap();
                assert!(name.starts_with(&format!("velotype-v1.2.3-{os}-{arch}.")));
            }
        }
        assert!(asset_name("1.2.3-rc.1", "windows", "x86_64").is_err());
        assert!(asset_name("1.2.3", "linux", "arm").is_err());
    }

    #[test]
    fn verifies_stream_size_digest_and_cancellation() {
        let digest = super::super::hex_digest(Sha256::digest(b"release"));
        let cancelled = AtomicBool::new(false);
        let check = |mut bytes: &[u8], size, hash: &str| {
            copy_verified(&mut bytes, &mut Vec::new(), size, hash, &cancelled)
        };
        assert!(check(b"release", 7, &digest).is_ok());
        assert!(check(b"releas", 7, &digest).is_err());
        assert!(check(b"releases", 7, &digest).is_err());
        assert!(check(b"changed", 7, &digest).is_err());
        cancelled.store(true, Ordering::Relaxed);
        assert!(check(b"release", 7, &digest).is_err());
    }

    #[test]
    fn rejects_traversal_and_platform_path_escapes() {
        for name in [
            "../velotype",
            "/velotype",
            "C:\\velotype.exe",
            "a/../../x",
            "a\\..\\x",
            "a:stream",
            "a./x",
        ] {
            assert!(safe_archive_path(name).is_err(), "{name}");
        }
        assert_eq!(
            safe_archive_path("./velotype").unwrap(),
            Path::new("velotype")
        );
    }

    #[test]
    fn updater_requires_an_exact_stable_asset_and_digest() {
        let fixture = || {
            Release {
            tag_name: "v1.2.3".into(), draft: false, prerelease: false,
            assets: vec![Asset {
                name: "velotype-v1.2.3-windows-x86_64.zip".into(), size: 10,
                digest: Some(format!("sha256:{}", "a".repeat(64))),
                browser_download_url: "https://github.com/manyougz/velotype/releases/download/v1.2.3/velotype-v1.2.3-windows-x86_64.zip".into(),
            }],
        }
        };
        assert!(select_asset(fixture(), "1.2.3", "windows", "x86_64").is_ok());
        assert!(select_asset(fixture(), "1.2.4", "windows", "x86_64").is_err());
        assert!(select_asset(fixture(), "1.2.3", "windows", "aarch64").is_err());
        let mut release = fixture();
        release.prerelease = true;
        assert!(select_asset(release, "1.2.3", "windows", "x86_64").is_err());
        let mut release = fixture();
        release.assets[0].digest = None;
        assert!(select_asset(release, "1.2.3", "windows", "x86_64").is_err());
        let mut release = fixture();
        release.assets[0].browser_download_url = "https://example.com/update.zip".into();
        assert!(select_asset(release, "1.2.3", "windows", "x86_64").is_err());
    }

    #[test]
    fn updater_zip_extraction_rejects_links_and_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("test.zip");
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        let cancelled = AtomicBool::new(false);
        let options = zip::write::FileOptions::<()>::default();
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer.start_file("velotype.exe", options.clone()).unwrap();
        writer.write_all(b"binary").unwrap();
        writer.start_file("README.md", options.clone()).unwrap();
        writer.write_all(b"ignored").unwrap();
        writer.finish().unwrap();
        extract_archive(&archive, &output, "windows", &cancelled).unwrap();
        assert_eq!(fs::read(output.join("velotype.exe")).unwrap(), b"binary");
        assert!(!output.join("README.md").exists());

        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .add_symlink("velotype.exe", "../outside", options.clone())
            .unwrap();
        writer.finish().unwrap();
        assert!(extract_archive(&archive, &output, "windows", &cancelled).is_err());
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer.start_file("../outside", options).unwrap();
        writer.write_all(b"escape").unwrap();
        writer.finish().unwrap();
        assert!(extract_archive(&archive, &output, "windows", &cancelled).is_err());
        assert!(!directory.path().join("outside").exists());
    }

    #[test]
    fn updater_tar_extraction_preserves_binary_and_rejects_links() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("test.tar.gz");
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        let write_archive = |link| {
            let mut writer = tar::Builder::new(flate2::write::GzEncoder::new(
                File::create(&archive).unwrap(),
                flate2::Compression::default(),
            ));
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            if link {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name("../outside").unwrap();
                header.set_size(0);
                header.set_cksum();
                writer
                    .append_data(&mut header, "velotype", std::io::empty())
                    .unwrap();
            } else {
                header.set_size(6);
                header.set_cksum();
                writer
                    .append_data(&mut header, "./velotype", &b"binary"[..])
                    .unwrap();
            }
            writer.into_inner().unwrap().finish().unwrap();
        };
        write_archive(false);
        extract_archive(&archive, &output, "linux", &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(output.join("velotype")).unwrap(), b"binary");
        write_archive(true);
        assert!(extract_archive(&archive, &output, "linux", &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn updater_macos_archive_keeps_the_entire_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("macos.zip");
        let output = directory.path().join("output");
        fs::create_dir(&output).unwrap();
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        for path in [
            "Velotype.app/Contents/MacOS/velotype",
            "Velotype.app/Contents/Info.plist",
            "Velotype.app/Contents/_CodeSignature/CodeResources",
        ] {
            writer
                .start_file(path, zip::write::FileOptions::<()>::default())
                .unwrap();
            writer.write_all(b"bundle data").unwrap();
        }
        writer.finish().unwrap();
        extract_archive(&archive, &output, "macos", &AtomicBool::new(false)).unwrap();
        assert!(
            output
                .join("Velotype.app/Contents/MacOS/velotype")
                .is_file()
        );
        assert!(output.join("Velotype.app/Contents/Info.plist").is_file());
        assert_eq!(
            fs::read(output.join("Velotype.app/Contents/_CodeSignature/CodeResources")).unwrap(),
            b"bundle data"
        );
    }

    #[test]
    fn updater_releases_the_lease_before_removing_prepared_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        let lease = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path.join(".lease"))
            .unwrap();
        lease.try_lock().unwrap();
        let prepared = PreparedUpdate {
            version: "1.2.3".into(),
            _lease: lease,
            directory,
            payload: path.join("payload"),
            target: InstallTarget {
                path: path.join("target"),
                executable: path.join("target"),
                bundle: false,
            },
        };
        drop(prepared);
        assert!(!path.exists());
    }

    #[test]
    fn updater_limits_ignored_archive_entries_before_decompression() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("oversized.tar.gz");
        let mut header = tar::Header::new_gnu();
        header.set_path("README.md").unwrap();
        header.set_mode(0o644);
        header.set_size(MAX_EXTRACTED_BYTES + 1);
        header.set_cksum();
        let mut writer = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        writer.write_all(header.as_bytes()).unwrap();
        writer.finish().unwrap();
        let error = extract_archive(&archive, directory.path(), "linux", &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    #[ignore = "requires GitHub network access; downloads but never installs"]
    fn updater_live_release_download_is_verified_and_prepared() {
        let prepared = prepare(env!("CARGO_PKG_VERSION"), &AtomicBool::new(false)).unwrap();
        assert!(prepared.payload.exists());
        assert_eq!(prepared.version, env!("CARGO_PKG_VERSION"));
    }
}
