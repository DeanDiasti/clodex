//! Stable GitHub releases, best-effort background checks, and atomic self-update.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use fs2::FileExt;
use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const LATEST_URL: &str = "https://api.github.com/repos/DeanDiasti/clodex/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/DeanDiasti/clodex/releases/download";
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Default, Deserialize, Serialize)]
struct CachedUpdate {
    checked_at: u64,
    release: Option<Release>,
}

impl Release {
    fn version(&self) -> Result<Version> {
        ensure!(!self.draft && !self.prerelease, "release is not stable");
        let version = Version::parse(self.tag_name.strip_prefix('v').unwrap_or(&self.tag_name))
            .context("release tag is not a semantic version")?;
        ensure!(version.pre.is_empty(), "release is a prerelease");
        Ok(version)
    }

    fn newer_than(&self, current: &str) -> bool {
        self.version()
            .and_then(|latest| Ok(latest.cmp_precedence(&Version::parse(current)?).is_gt()))
            .unwrap_or(false)
    }

    fn asset(&self, name: &str) -> Result<&Asset> {
        let asset = self
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .with_context(|| format!("release {} has no {name} asset", self.tag_name))?;
        // Only the project's own release URLs are allowed, including in the cache.
        ensure!(
            asset.browser_download_url == format!("{DOWNLOAD_BASE}/{}/{name}", self.tag_name),
            "unexpected release download URL"
        );
        Ok(asset)
    }

    fn archive_name(&self) -> Result<String> {
        Ok(format!(
            "clodex-{}-{}.tar.gz",
            self.tag_name,
            release_target()?
        ))
    }

    fn installable(&self) -> bool {
        self.archive_name()
            .and_then(|name| self.asset(&name))
            .is_ok()
            && self.asset("SHA256SUMS").is_ok()
    }
}

fn release_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("x86_64-unknown-linux-gnu"),
        _ => bail!(
            "no prebuilt Clodex release for this platform; update from source with scripts/install.sh"
        ),
    }
}

fn client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("clodex/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .https_only(true)
        .build()?)
}

fn fetch_latest(client: &Client, url: &str) -> Result<Option<Release>> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2026-03-10")
        .send()
        .context("could not check GitHub for Clodex updates")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release: Release = serde_json::from_reader(response.error_for_status()?.take(1024 * 1024))?;
    release.version()?;
    Ok(Some(release))
}

fn cache_path() -> Result<PathBuf> {
    Ok(crate::config::clodex_home()?
        .join("cache")
        .join("updates.json"))
}

fn read_cache(path: &Path) -> CachedUpdate {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_cache(path: &Path, cache: &CachedUpdate) -> Result<()> {
    let directory = path.parent().context("update cache has no parent")?;
    fs::create_dir_all(directory)?;
    let mut temporary = NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, cache)?;
    temporary.persist(path)?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn is_fresh(cache: &CachedUpdate, now: u64) -> bool {
    now >= cache.checked_at && now - cache.checked_at < CHECK_INTERVAL.as_secs()
}

/// Status-line invocations only read the cache; network failures never affect them.
pub fn available_label() -> Option<String> {
    let release = read_cache(&cache_path().ok()?).release?;
    (release.newer_than(env!("CARGO_PKG_VERSION")) && release.installable()).then(|| {
        format!(
            "Clodex {} update available · clodex update",
            release.tag_name
        )
    })
}

/// Runs alongside Claude, including long sessions. Other sessions share the cache
/// and a nonblocking lock, so only one performs each hourly check.
pub fn start_background_checks() {
    let _ = std::thread::Builder::new()
        .name("clodex-update-check".into())
        .spawn(|| {
            loop {
                let _ = check_in_background();
                std::thread::sleep(CHECK_INTERVAL);
            }
        });
}

fn check_in_background() -> Result<()> {
    check_cached(&cache_path()?, || {
        fetch_latest(&client(Duration::from_secs(5))?, LATEST_URL)
    })
}

fn check_cached(path: &Path, fetch: impl FnOnce() -> Result<Option<Release>>) -> Result<()> {
    if is_fresh(&read_cache(path), now_secs()) {
        return Ok(());
    }
    fs::create_dir_all(path.parent().context("update cache has no parent")?)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let mut cache = read_cache(path);
    if is_fresh(&cache, now_secs()) {
        return Ok(());
    }
    // Record failed attempts too, avoiding repeated offline/rate-limited requests.
    if let Ok(release) = fetch() {
        cache.release = release;
    }
    cache.checked_at = now_secs();
    write_cache(path, &cache)
}

pub fn run() -> Result<()> {
    release_target()?;
    println!("Checking for Clodex updates...");
    let client = client(Duration::from_secs(120))?;
    let release = fetch_latest(&client, LATEST_URL)?;
    if let Ok(path) = cache_path() {
        let _ = write_cache(
            &path,
            &CachedUpdate {
                checked_at: now_secs(),
                release: release.clone(),
            },
        );
    }
    let Some(release) = release else {
        println!("No published Clodex releases are available yet.");
        return Ok(());
    };
    if !release.newer_than(env!("CARGO_PKG_VERSION")) {
        println!("Clodex {} is up to date.", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let archive_name = release.archive_name()?;
    let archive_asset = release.asset(&archive_name)?;
    let checksum_asset = release.asset("SHA256SUMS")?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let directory = executable
        .parent()
        .context("Clodex executable has no parent directory")?;
    // Check destination permissions before downloading; stage on the same filesystem.
    let staged = NamedTempFile::new_in(directory).with_context(|| {
        format!(
            "cannot update {}; its directory must be writable",
            executable.display()
        )
    })?;

    println!("Downloading Clodex {}...", release.tag_name);
    let mut checksum_bytes = Vec::new();
    client
        .get(&checksum_asset.browser_download_url)
        .send()?
        .error_for_status()?
        .take(1024 * 1024)
        .read_to_end(&mut checksum_bytes)?;
    let checksums = std::str::from_utf8(&checksum_bytes).context("invalid release checksums")?;
    let mut archive = NamedTempFile::new()?;
    let copied = io::copy(
        &mut client
            .get(&archive_asset.browser_download_url)
            .send()?
            .error_for_status()?
            .take(MAX_ARCHIVE_BYTES + 1),
        &mut archive,
    )?;
    ensure!(
        copied <= MAX_ARCHIVE_BYTES,
        "release archive exceeds the size limit"
    );
    verify_checksum(archive.as_file_mut(), checksums, &archive_name)?;
    install_archive(
        archive.as_file_mut(),
        staged,
        &executable,
        &release.version()?.to_string(),
    )?;
    println!(
        "Updated Clodex to {} at {}.",
        release.tag_name,
        executable.display()
    );
    println!(
        "New Clodex sessions will use this update. Existing sessions keep their deployment until they end."
    );
    Ok(())
}

fn verify_checksum(archive: &mut File, checksums: &str, name: &str) -> Result<()> {
    let expected = checksums
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            (fields.next()?.trim_start_matches('*') == name).then_some(hash)
        })
        .with_context(|| format!("SHA256SUMS has no checksum for {name}"))?;
    ensure!(
        expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA256 checksum"
    );
    archive.rewind()?;
    let mut hash = Sha256::new();
    io::copy(archive, &mut hash)?;
    let actual = format!("{:x}", hash.finalize());
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "release archive checksum mismatch; the installed binary was not changed"
    );
    Ok(())
}

fn install_archive(
    archive: &mut File,
    mut staged: NamedTempFile,
    executable: &Path,
    version: &str,
) -> Result<()> {
    archive.rewind()?;
    let mut tar = tar::Archive::new(GzDecoder::new(archive));
    let mut found = false;
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.as_ref() != Path::new("clodex") {
            continue;
        }
        ensure!(
            !found && entry.header().entry_type().is_file(),
            "release must contain one regular clodex binary"
        );
        ensure!(
            entry.size() > 0 && entry.size() <= MAX_ARCHIVE_BYTES,
            "invalid release binary size"
        );
        io::copy(&mut entry, staged.as_file_mut())?;
        found = true;
    }
    ensure!(found, "release archive has no clodex binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    staged.as_file_mut().flush()?;
    staged.as_file().sync_all()?;
    // Close the writable handle before execution (Linux otherwise returns ETXTBSY).
    let staged = staged.into_temp_path();
    let output = Command::new(&staged)
        .arg("--version")
        .output()
        .context("downloaded Clodex binary cannot run on this machine")?;
    ensure!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == format!("clodex {version}"),
        "downloaded binary version does not match the release; the installed binary was not changed"
    );
    // persist atomically replaces the directory entry, never the running inode.
    // A symlink invocation was canonicalized by the caller and keeps its link.
    staged
        .persist(executable)
        .context("could not replace the installed Clodex binary")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::net::TcpListener;

    fn release(tag: &str) -> Release {
        let mut release = Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: vec![],
        };
        for name in [release.archive_name().unwrap(), "SHA256SUMS".into()] {
            release.assets.push(Asset {
                browser_download_url: format!("{DOWNLOAD_BASE}/{tag}/{name}"),
                name,
            });
        }
        release
    }

    #[test]
    fn notices_use_semantic_versions_and_only_installable_stable_releases() {
        assert!(release("v0.10.0").newer_than("0.9.0"));
        assert!(!release("v0.9.0").newer_than("0.10.0"));
        assert!(!release("v1.0.0+new").newer_than("1.0.0+old"));
        assert!(!release("v1.0.0-rc.1").newer_than("0.1.0"));
        assert!(!release("not-a-version").newer_than("0.1.0"));
        let mut latest = release("v1.0.0");
        assert!(latest.installable());
        latest.prerelease = true;
        assert!(!latest.newer_than("0.1.0"));
        latest.prerelease = false;
        latest.assets[0].browser_download_url = "https://example.org/binary".into();
        assert!(!latest.installable());
        latest.assets.clear();
        assert!(!latest.installable());
    }

    #[test]
    fn hourly_cache_reuses_results_and_throttles_failed_checks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache/updates.json");
        check_cached(&path, || Ok(Some(release("v1.0.0")))).unwrap();
        check_cached(&path, || panic!("a fresh cache must not fetch again")).unwrap();
        let mut cache = read_cache(&path);
        assert_eq!(cache.release.as_ref().unwrap().tag_name, "v1.0.0");
        assert!(!is_fresh(
            &cache,
            cache.checked_at + CHECK_INTERVAL.as_secs()
        ));
        assert!(!is_fresh(&cache, cache.checked_at - 1));
        cache.checked_at = 0;
        write_cache(&path, &cache).unwrap();
        check_cached(&path, || bail!("offline")).unwrap();
        assert!(is_fresh(&read_cache(&path), now_secs()));
        assert_eq!(read_cache(&path).release.unwrap().tag_name, "v1.0.0");
        check_cached(&path, || panic!("failed attempts must be throttled")).unwrap();
        fs::write(&path, "broken JSON").unwrap();
        assert!(read_cache(&path).release.is_none());
    }

    #[test]
    fn concurrent_checker_does_not_wait_for_the_cache_lock() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("updates.json");
        let lock = File::create(path.with_extension("lock")).unwrap();
        lock.lock_exclusive().unwrap();
        check_cached(&path, || panic!("another checker owns the lock")).unwrap();
        assert!(!path.exists());
    }

    fn fake_github(status: &str, body: &str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/latest", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("GET /latest "));
            assert!(
                request
                    .to_lowercase()
                    .contains("accept: application/vnd.github+json")
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (url, thread)
    }

    #[test]
    fn github_lookup_handles_new_releases_missing_releases_and_failures() {
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for (status, body, expected) in [
            (
                "200 OK",
                serde_json::to_string(&release("v1.0.0")).unwrap(),
                "release",
            ),
            ("404 Not Found", "{}".into(), "none"),
            ("403 Forbidden", "{}".into(), "error"),
            ("200 OK", "invalid JSON".into(), "error"),
            (
                "200 OK",
                serde_json::to_string(&release("v1.0.0-rc.1")).unwrap(),
                "error",
            ),
        ] {
            let (url, server) = fake_github(status, &body);
            let result = fetch_latest(&client, &url);
            server.join().unwrap();
            match expected {
                "release" => assert_eq!(result.unwrap().unwrap().tag_name, "v1.0.0"),
                "none" => assert!(result.unwrap().is_none()),
                _ => assert!(result.is_err()),
            }
        }
    }

    fn archive(entries: &[(&str, &[u8], tar::EntryType)]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        let gzip = GzEncoder::new(file.as_file_mut(), Compression::default());
        let mut tar = tar::Builder::new(gzip);
        for (path, bytes, kind) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(*kind);
            header.set_cksum();
            tar.append_data(&mut header, path, *bytes).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        file
    }

    #[cfg(unix)]
    #[test]
    fn verified_archive_atomically_replaces_the_binary_and_preserves_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("clodex");
        fs::write(&executable, "old binary").unwrap();
        let mut old_inode = File::open(&executable).unwrap();
        let link = directory.path().join("linked-clodex");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        let binary = b"#!/bin/sh\nprintf 'clodex 1.2.3\\n'\n";
        let mut archive = archive(&[
            ("clodex", binary, tar::EntryType::Regular),
            ("README.md", b"notes", tar::EntryType::Regular),
        ]);
        let hash = format!("{:x}", Sha256::digest(fs::read(archive.path()).unwrap()));
        verify_checksum(
            archive.as_file_mut(),
            &format!("{hash}  release.tar.gz\n"),
            "release.tar.gz",
        )
        .unwrap();
        install_archive(
            archive.as_file_mut(),
            NamedTempFile::new_in(directory.path()).unwrap(),
            &link.canonicalize().unwrap(),
            "1.2.3",
        )
        .unwrap();
        assert_eq!(fs::read(&executable).unwrap(), binary);
        assert_eq!(fs::read(&link).unwrap(), binary);
        assert!(link.is_symlink());
        let mut old = String::new();
        old_inode.read_to_string(&mut old).unwrap();
        assert_eq!(old, "old binary");
        assert!(!directory.path().join("README.md").exists());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn bad_checksum_version_and_archive_leave_the_old_binary_intact() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("clodex");
        fs::write(&executable, "old binary").unwrap();
        let binary = b"#!/bin/sh\nprintf 'clodex 1.0.0\\n'\n";
        let mut good = archive(&[("clodex", binary, tar::EntryType::Regular)]);
        assert!(
            verify_checksum(
                good.as_file_mut(),
                &format!("{}  archive\n", "0".repeat(64)),
                "archive"
            )
            .is_err()
        );
        assert!(verify_checksum(good.as_file_mut(), "bad  archive", "archive").is_err());
        assert!(verify_checksum(good.as_file_mut(), "", "archive").is_err());
        for entries in [
            vec![("clodex", binary.as_slice(), tar::EntryType::Regular)],
            vec![("clodex", b"".as_slice(), tar::EntryType::Symlink)],
            vec![("README.md", b"notes".as_slice(), tar::EntryType::Regular)],
            vec![("clodex", binary.as_slice(), tar::EntryType::Regular); 2],
        ] {
            let mut archive = archive(&entries);
            let staged = NamedTempFile::new_in(directory.path()).unwrap();
            assert!(install_archive(archive.as_file_mut(), staged, &executable, "2.0.0").is_err());
            assert_eq!(fs::read_to_string(&executable).unwrap(), "old binary");
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }
}
