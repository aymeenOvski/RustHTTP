//! writes big request bodies to a temporary file.
//!
//! A body bigger than `SPOOL_ABOVE` is written to disk piece by piece while
//! it arrives, so RAM holds only one piece at a time, whatever the upload
//! size. The router then gets it as `Request::body_file`.
//!
//! The files go in `SPOOL_DIR` inside the project folder, not the system
//! temp folder: on some Linux systems /tmp lives in RAM, which would defeat
//! the purpose.
//!
//! Deleting is done in two parts. The name is removed at once; the disk
//! space is freed a piece per turn of the event loop (`throw_away`), because
//! the kernel can take up to a second to free 1 GB in one go, and the whole
//! server would stop during that second.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::http::BodyFile;

/// Bodies bigger than this go to a file; smaller ones stay in memory.
pub const SPOOL_ABOVE: usize = 1024 * 1024;
/// Where the temporary body files live (relative to where the server runs).
pub const SPOOL_DIR: &str = "tmp";
/// Disk space freed per turn of the event loop (a few ms of kernel work).
const FREE_PER_TURN: u64 = 16 * 1024 * 1024;

/// Makes every file name unique.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Deleted files (no name any more) whose space is not freed yet.
static TRASH: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// A body file being written. Dropped before `finish` (client gone, body too
/// big, broken chunks): the half-written file is deleted.
pub struct Spool {
    /// `None` once `finish` handed the file over.
    file: Option<File>,
    path: PathBuf,
    len: u64,
}

impl Spool {
    pub fn create() -> io::Result<Spool> {
        fs::create_dir_all(SPOOL_DIR)?;
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(SPOOL_DIR).join(format!("body-{}-{n}.tmp", std::process::id()));
        // Read too: the same handle is used later to read the body back.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Spool {
            file: Some(file),
            path,
            len: 0,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let file = self.file.as_mut().ok_or(io::ErrorKind::NotFound)?;
        file.write_all(bytes)?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The whole body is written: hand the file over to the request.
    pub fn finish(mut self) -> io::Result<BodyFile> {
        let mut file = self.file.take().ok_or(io::ErrorKind::NotFound)?;
        file.flush()?;
        file.seek(SeekFrom::Start(0))?; // the reader starts at the beginning
        Ok(BodyFile::new(self.path.clone(), self.len, file))
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = fs::remove_file(&self.path);
            throw_away(file);
        }
    }
}

/// Takes a body file whose name is deleted and that nobody else reads, and
/// frees its disk space later, a piece per turn (`free_some_space`).
pub fn throw_away(file: File) {
    let big = file.metadata().map(|m| m.len() > FREE_PER_TURN).unwrap_or(false);
    if !big {
        return; // small: closing it now is quick (the file is dropped here)
    }
    match TRASH.lock() {
        Ok(mut trash) => trash.push(file),
        Err(_) => {} // cannot happen (nothing panics while holding the lock)
    }
}

/// Frees up to FREE_PER_TURN bytes of thrown-away files, by making them
/// shorter. Called once per turn of the event loop.
/// Returns true while there is still something to free.
pub fn free_some_space() -> bool {
    let Ok(mut trash) = TRASH.lock() else {
        return false;
    };
    let Some(file) = trash.last() else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let shorter = len.saturating_sub(FREE_PER_TURN);
    if shorter == 0 || file.set_len(shorter).is_err() {
        trash.pop(); // closed here: little or nothing left to free
    }
    !trash.is_empty()
}

/// Deletes body files left behind by a server that was killed (kill -9,
/// crash of the machine): `Drop` never ran for them. Called at startup.
pub fn clean_old_files() {
    let Ok(entries) = fs::read_dir(SPOOL_DIR) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("body-") && name.ends_with(".tmp") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn finished_spool_becomes_a_body_file() {
        let mut spool = Spool::create().unwrap();
        spool.write(b"hello ").unwrap();
        spool.write(b"world").unwrap();
        let body = spool.finish().unwrap();
        assert_eq!(body.len(), 11);
        // The job gets the open file, at its start.
        let mut text = String::new();
        body.take_file().unwrap().read_to_string(&mut text).unwrap();
        assert_eq!(text, "hello world");
        let path = body.path().to_path_buf();
        drop(body); // request finished
        assert!(!path.exists(), "the temp file must be deleted");
    }

    #[test]
    fn unfinished_spool_is_deleted() {
        let mut spool = Spool::create().unwrap();
        spool.write(b"half a body").unwrap();
        let path = spool.path.clone();
        assert!(path.exists());
        drop(spool); // client gone mid-upload
        assert!(!path.exists());
    }

    #[test]
    fn a_big_thrown_away_file_is_freed_in_pieces() {
        let path = std::env::temp_dir().join(format!("trash_{}.bin", std::process::id()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(5 * FREE_PER_TURN).unwrap();
        let _ = fs::remove_file(&path);
        throw_away(file);
        // A few turns to free it (other tests may share the trash).
        let mut turns = 0;
        while free_some_space() {
            turns += 1;
            assert!(turns < 1000, "the trash never empties");
        }
        assert!(turns >= 4, "freed in {turns} turns");
    }
}
