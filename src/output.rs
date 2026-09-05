//! Safe output writing.
//!
//! Three guarantees, because this tool writes files derived from untrusted
//! input into a directory the user cares about:
//!
//! 1. **Never clobber by default.** Existing targets are detected *before* any
//!    write happens, so a refused run leaves the directory untouched. This is
//!    a check-then-write, not an atomic claim: a file created by another
//!    process in the window between the check and the rename is overwritten.
//!    That race is not worth a lockfile for a single-user CLI, but it is a
//!    real gap and is stated here rather than papered over.
//! 2. **Atomic.** Each file is written to a temporary file in the destination
//!    directory and then renamed into place, so a crash or a full disk can
//!    never leave a half-written `context.md` behind.
//! 3. **All-or-nothing pre-flight.** A [`Writer`] validates every staged target
//!    up front, so a package is never partially created because the third of
//!    four files already existed.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::text;

/// Collects the files of an output package, then commits them together.
#[derive(Debug)]
pub struct Writer {
    /// Destination directory.
    pub dir: PathBuf,
    /// Whether existing files may be overwritten.
    pub force: bool,
    staged: Vec<(String, String)>,
}

impl Writer {
    pub fn new(dir: impl Into<PathBuf>, force: bool) -> Self {
        Self {
            dir: dir.into(),
            force,
            staged: Vec::new(),
        }
    }

    /// Queue a file. `name` is reduced to a single safe path component, so a
    /// caller can never escape the output directory.
    ///
    /// Staging the same sanitized name twice replaces the earlier entry rather
    /// than writing one path twice and reporting it twice. Two distinct names
    /// can sanitize to one component, so this is not reachable only through
    /// caller error.
    pub fn stage(&mut self, name: &str, contents: String) {
        let name = text::sanitize_filename(name);
        match self
            .staged
            .iter_mut()
            .find(|(existing, _)| *existing == name)
        {
            Some(slot) => slot.1 = contents,
            None => self.staged.push((name, contents)),
        }
    }

    /// Names staged so far, in insertion order.
    pub fn staged_names(&self) -> impl Iterator<Item = &str> {
        self.staged.iter().map(|(name, _)| name.as_str())
    }

    /// Validate every target, create the directory, then write all files
    /// atomically. Returns the paths written, in staging order.
    pub fn commit(self) -> Result<Vec<PathBuf>> {
        let targets: Vec<PathBuf> = self
            .staged
            .iter()
            .map(|(name, _)| self.dir.join(name))
            .collect();

        if !self.force {
            for target in &targets {
                if target.exists() {
                    return Err(Error::OutputExists {
                        path: target.clone(),
                    });
                }
            }
        }

        std::fs::create_dir_all(&self.dir).map_err(|e| Error::write(&self.dir, e))?;

        let mut written = Vec::with_capacity(targets.len());
        for (target, (_, contents)) in targets.iter().zip(self.staged.iter()) {
            write_atomic_unchecked(target, contents)?;
            written.push(target.clone());
        }
        Ok(written)
    }
}

/// Write a single file atomically, refusing to overwrite unless `force`.
pub fn write_atomic(path: &Path, contents: &str, force: bool) -> Result<()> {
    if !force && path.exists() {
        return Err(Error::OutputExists {
            path: path.to_path_buf(),
        });
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::write(parent, e))?;
    }
    write_atomic_unchecked(path, contents)
}

/// Write-then-rename. The temporary file is created in the destination
/// directory so the rename stays on one filesystem and is therefore atomic.
fn write_atomic_unchecked(path: &Path, contents: &str) -> Result<()> {
    let dir = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => parent,
        None => Path::new("."),
    };

    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(|e| Error::write(path, e))?;
    temp.write_all(contents.as_bytes())
        .map_err(|e| Error::write(path, e))?;
    temp.flush().map_err(|e| Error::write(path, e))?;
    temp.persist(path)
        .map_err(|e| Error::write(path, e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(e) => panic!("test needs a temp dir: {e}"),
        }
    }

    #[test]
    fn commit_writes_all_staged_files() {
        let dir = temp_dir();
        let out = dir.path().join("handoff");
        let mut writer = Writer::new(&out, false);
        writer.stage("context.md", "ctx".into());
        writer.stage("transcript.md", "tx".into());

        let written = writer.commit().expect("fresh directory must be writable");
        assert_eq!(written.len(), 2);
        assert_eq!(
            std::fs::read_to_string(out.join("context.md")).ok(),
            Some("ctx".to_string())
        );
        assert_eq!(
            std::fs::read_to_string(out.join("transcript.md")).ok(),
            Some("tx".to_string())
        );
    }

    #[test]
    fn commit_refuses_to_clobber_and_writes_nothing() {
        let dir = temp_dir();
        let out = dir.path().join("handoff");
        std::fs::create_dir_all(&out).expect("setup");
        std::fs::write(out.join("context.md"), "original").expect("setup");

        let mut writer = Writer::new(&out, false);
        writer.stage("context.md", "new".into());
        writer.stage("transcript.md", "new".into());

        let err = writer.commit().expect_err("must refuse to overwrite");
        assert!(matches!(err, Error::OutputExists { .. }));
        assert!(format!("{err}").contains("--force"));
        // The pre-flight check must run before any write: the file that did not
        // previously exist must still not exist.
        assert!(!out.join("transcript.md").exists());
        assert_eq!(
            std::fs::read_to_string(out.join("context.md")).ok(),
            Some("original".to_string())
        );
    }

    #[test]
    fn force_overwrites() {
        let dir = temp_dir();
        let out = dir.path().join("handoff");
        std::fs::create_dir_all(&out).expect("setup");
        std::fs::write(out.join("context.md"), "original").expect("setup");

        let mut writer = Writer::new(&out, true);
        writer.stage("context.md", "replaced".into());
        writer.commit().expect("force must overwrite");
        assert_eq!(
            std::fs::read_to_string(out.join("context.md")).ok(),
            Some("replaced".to_string())
        );
    }

    #[test]
    fn staged_names_cannot_escape_the_output_directory() {
        let dir = temp_dir();
        let out = dir.path().join("handoff");
        let mut writer = Writer::new(&out, false);
        writer.stage("../../escape.md", "nope".into());
        let names: Vec<&str> = writer.staged_names().collect();
        assert!(!names[0].contains('/'));

        let written = writer.commit().expect("write");
        for path in &written {
            assert!(path.starts_with(&out), "{path:?} escaped {out:?}");
        }
        assert!(!dir.path().join("escape.md").exists());
    }

    #[test]
    fn staging_a_colliding_name_replaces_rather_than_duplicating() {
        let dir = temp_dir();
        let out = dir.path().join("handoff");
        let mut writer = Writer::new(&out, false);
        // Two distinct names that sanitize to the same component.
        writer.stage("a/b.md", "first".into());
        writer.stage("a:b.md", "second".into());
        assert_eq!(writer.staged_names().count(), 1);

        let written = writer.commit().expect("write");
        assert_eq!(written.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&written[0]).ok(),
            Some("second".to_string())
        );
    }

    #[test]
    fn write_atomic_creates_parent_directories() {
        let dir = temp_dir();
        let target = dir.path().join("a/b/c/out.md");
        write_atomic(&target, "hello", false).expect("nested write");
        assert_eq!(
            std::fs::read_to_string(&target).ok(),
            Some("hello".to_string())
        );
    }

    #[test]
    fn write_atomic_refuses_existing_file_without_force() {
        let dir = temp_dir();
        let target = dir.path().join("out.md");
        write_atomic(&target, "first", false).expect("first write");
        let err = write_atomic(&target, "second", false).expect_err("must refuse");
        assert!(matches!(err, Error::OutputExists { .. }));
        assert_eq!(
            std::fs::read_to_string(&target).ok(),
            Some("first".to_string())
        );
        write_atomic(&target, "second", true).expect("force write");
        assert_eq!(
            std::fs::read_to_string(&target).ok(),
            Some("second".to_string())
        );
    }

    #[test]
    fn unicode_content_round_trips_as_utf8() {
        let dir = temp_dir();
        let target = dir.path().join("he.md");
        let body = "איבוגה גמילה מאופיאטים 👨‍👩‍👧‍👦";
        write_atomic(&target, body, false).expect("write");
        assert_eq!(
            std::fs::read_to_string(&target).ok(),
            Some(body.to_string())
        );
    }

    #[test]
    fn no_temporary_files_are_left_behind() {
        let dir = temp_dir();
        let out = dir.path().join("handoff");
        let mut writer = Writer::new(&out, false);
        writer.stage("context.md", "ctx".into());
        writer.commit().expect("write");

        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .expect("readdir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "context.md")
            .collect();
        assert!(leftovers.is_empty(), "stray files: {leftovers:?}");
    }
}
