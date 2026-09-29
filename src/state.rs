use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub struct State {
    pub root: PathBuf,
}

impl State {
    pub fn new() -> io::Result<Self> {
        let root = match std::env::var_os("LOOPSIE_DIR") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            Some(_) => return Err(io::Error::other("LOOPSIE_DIR must not be empty")),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .ok_or_else(|| io::Error::other("set HOME or LOOPSIE_DIR"))?,
            )
            .join(".loopsie"),
        };
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        Ok(Self { root })
    }
    pub fn ensure(&self) -> io::Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)
    }
    pub fn path(&self, name: &str, suffix: &str) -> PathBuf {
        self.root.join(format!("{name}.{suffix}"))
    }
    pub fn lock(&self, name: &str) -> io::Result<File> {
        let path = self.path(name, "lock");
        loop {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            file.try_lock().map_err(|err| match err {
                TryLockError::WouldBlock => io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("loop '{name}' is already running"),
                ),
                TryLockError::Error(err) => err,
            })?;
            // Prune unlinks locks while holding them. A lock taken on an
            // unlinked inode guards nothing, so retry on the current path.
            let held = file.metadata()?;
            match fs::symlink_metadata(&path) {
                Ok(linked) if linked.dev() == held.dev() && linked.ino() == held.ino() => {
                    return Ok(file);
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }
    }
    pub fn is_running(&self, name: &str) -> io::Result<bool> {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path(name, "lock"))
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }
    pub fn names(&self) -> io::Result<Vec<String>> {
        names_with_extension(&self.root, "lock")
    }
    /// Removes a loop's state and logs unless it is running; returns whether it did.
    pub fn remove_stopped(&self, name: &str) -> io::Result<bool> {
        let _lock = match self.lock(name) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(err) => return Err(err),
        };
        // Names cannot contain dots, so the first dot ends the owner's name.
        // Legacy PID files are left for the user, as `run` asks.
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            let owned = path
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|file| file.split_once('.'))
                .is_some_and(|(owner, suffix)| {
                    owner == name && suffix != "lock" && suffix != "pid"
                });
            if owned {
                remove_existing(&path)?;
            }
        }
        // Unlink the lock last, while holding it, so a failure leaves the loop listed.
        remove_existing(&self.path(name, "lock"))?;
        Ok(true)
    }
    pub fn write_status(&self, name: &str, status: &str) -> io::Result<()> {
        atomic_write(&self.path(name, "status"), status.as_bytes())
    }
    pub fn set_alias(&self, name: &str, command: &[OsString]) -> io::Result<()> {
        self.ensure()?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.root.join("aliases"))?;
        let mut bytes = Vec::new();
        for arg in command {
            bytes.extend_from_slice(arg.as_bytes());
            bytes.push(0);
        }
        atomic_write(
            &self.root.join("aliases").join(format!("{name}.argv")),
            &bytes,
        )
    }
    pub fn alias(&self, name: &str) -> io::Result<Vec<OsString>> {
        let bytes = fs::read(self.root.join("aliases").join(format!("{name}.argv")))?;
        if bytes.last() != Some(&0) || bytes.len() == 1 {
            return Err(io::Error::other("invalid alias file"));
        }
        Ok(bytes[..bytes.len() - 1]
            .split(|b| *b == 0)
            .map(|b| OsString::from_vec(b.to_vec()))
            .collect())
    }
    pub fn alias_names(&self) -> io::Result<Vec<String>> {
        names_with_extension(&self.root.join("aliases"), "argv")
    }
    pub fn remove_alias(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.root.join("aliases").join(format!("{name}.argv")))
    }
}

fn names_with_extension(root: &Path, extension: &str) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension() == Some(OsStr::new(extension))
            && let Some(name) = path.file_stem().and_then(OsStr::to_str)
            && crate::cli::valid_name(name).is_ok()
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn remove_existing(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temp_name = path.as_os_str().to_os_string();
    temp_name.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp_name);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        file.write_all(bytes)?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
