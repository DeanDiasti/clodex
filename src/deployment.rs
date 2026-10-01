//! Pin the executable inode and share a runtime only with the same binary build.

use std::fs::{self, File};
use std::io::{self, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

pub struct Deployment {
    id: String,
    // Keep the original inode open even if an update replaces its installed path.
    source: Mutex<File>,
}

impl Deployment {
    pub fn current() -> Result<&'static Self> {
        static CURRENT: OnceLock<Deployment> = OnceLock::new();
        if let Some(deployment) = CURRENT.get() {
            return Ok(deployment);
        }
        let deployment = Self::open(&std::env::current_exe()?)?;
        let _ = CURRENT.set(deployment);
        Ok(CURRENT.get().expect("deployment was initialized"))
    }

    fn open(executable: &Path) -> Result<Self> {
        let mut source = File::open(executable)
            .with_context(|| format!("could not pin {}", executable.display()))?;
        let mut hash = Sha256::new();
        io::copy(&mut source, &mut hash)?;
        source.rewind()?;
        Ok(Self {
            id: format!("{:x}", hash.finalize()),
            source: Mutex::new(source),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn runtime_directory(&self, home: &Path) -> PathBuf {
        // Keep Unix socket paths short, including on macOS (104-byte limit).
        home.join("run").join(&self.id[..24])
    }

    /// Supervisors and status-line helpers must run this session's original
    /// build, even if the installed executable is replaced before they start.
    pub fn snapshot(&self, runtime: &Path) -> Result<PathBuf> {
        fs::create_dir_all(runtime)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(runtime, fs::Permissions::from_mode(0o700))?;
        }
        let executable = runtime.join("clodex");
        if executable.is_file() {
            return Ok(executable);
        }
        let mut staged = NamedTempFile::new_in(runtime)?;
        let mut source = self
            .source
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment source lock was poisoned"))?;
        source.rewind()?;
        io::copy(&mut *source, &mut staged)?;
        staged.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            staged
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        match staged.persist_noclobber(&executable) {
            Ok(_) => Ok(executable),
            // Another launcher finished the identical snapshot first.
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(executable),
            Err(error) => Err(error).context("could not save the deployment executable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_the_installed_binary_keeps_the_original_build_pinned() {
        let directory = tempfile::tempdir().unwrap();
        let installed = directory.path().join("clodex");
        fs::write(&installed, b"old build").unwrap();
        let old = Deployment::open(&installed).unwrap();
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, b"new build, same package version").unwrap();
        fs::rename(replacement, &installed).unwrap();
        let new = Deployment::open(&installed).unwrap();
        assert_ne!(old.id(), new.id());
        let old_runtime = old.runtime_directory(directory.path());
        let new_runtime = new.runtime_directory(directory.path());
        assert_ne!(old_runtime, new_runtime);
        assert_eq!(
            fs::read(old.snapshot(&old_runtime).unwrap()).unwrap(),
            b"old build"
        );
        assert_eq!(
            fs::read(new.snapshot(&new_runtime).unwrap()).unwrap(),
            b"new build, same package version"
        );
        // The same binary at another path must share the deployment.
        assert_eq!(
            Deployment::open(&old_runtime.join("clodex")).unwrap().id(),
            old.id()
        );
    }
}
