//! Byte-addressed files: what the database file and the WAL are stored on.
//!
//! A real file, or a vector of bytes for a database that lives in memory.
//! Writes may be lost in a crash until `sync` returns.

use std::fs::File;
use std::sync::RwLock;

use crate::error::{Result, err};

pub trait Medium: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// Writes past the end extend the medium, zero-filling any gap.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()>;
    fn size(&self) -> Result<u64>;
    /// Cuts the medium to `len` bytes, or zero-extends it.
    fn set_len(&self, len: u64) -> Result<()>;
    /// Makes every earlier write durable.
    fn sync(&self) -> Result<()>;
}

/// File-backed storage using positional I/O, so no shared cursor is needed.
pub struct FileMedium(pub File);

impl Medium for FileMedium {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        Ok(positional::read_exact_at(&self.0, buf, offset)?)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        Ok(positional::write_all_at(&self.0, buf, offset)?)
    }

    fn size(&self) -> Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn set_len(&self, len: u64) -> Result<()> {
        Ok(self.0.set_len(len)?)
    }

    fn sync(&self) -> Result<()> {
        Ok(self.0.sync_data()?)
    }
}

#[cfg(unix)]
mod positional {
    use std::fs::File;
    use std::io;
    use std::os::unix::fs::FileExt;

    pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        file.read_exact_at(buf, offset)
    }

    pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
        file.write_all_at(buf, offset)
    }
}

/// Windows has no `read_exact_at`: `seek_read` and `seek_write` may transfer
/// fewer bytes, so they are looped. They also move the file's cursor, which
/// nothing here uses.
#[cfg(windows)]
mod positional {
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::FileExt;

    pub fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match file.seek_read(buf, offset) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match file.seek_write(buf, offset) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct MemMedium(RwLock<Vec<u8>>);

impl Medium for MemMedium {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let bytes = self.0.read().unwrap();
        match bytes.get(offset as usize..offset as usize + buf.len()) {
            Some(data) => buf.copy_from_slice(data),
            None => return err(format!("read past the end at {offset}")),
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut bytes = self.0.write().unwrap();
        let end = offset as usize + buf.len();
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[offset as usize..end].copy_from_slice(buf);
        Ok(())
    }

    fn size(&self) -> Result<u64> {
        Ok(self.0.read().unwrap().len() as u64)
    }

    fn set_len(&self, len: u64) -> Result<()> {
        self.0.write().unwrap().resize(len as usize, 0);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub mod sim {
    //! Files that can crash, for testing recovery.
    //!
    //! A [`Sim`] holds several files. Each write, resize and sync counts as an
    //! operation; after a set number of them every operation fails, as if the
    //! machine had stopped (or just one does, as an I/O error would). [`Sim::crashed`] then returns what a restarted
    //! machine would find: each file as of its last sync, plus the writes
    //! after that which happened to reach the disk.

    use std::sync::{Arc, Mutex};

    use super::Medium;
    use crate::error::{Result, err};

    #[derive(Clone, Debug)]
    enum Op {
        Write(u64, Vec<u8>),
        SetLen(u64),
    }

    #[derive(Default, Clone)]
    struct SimFile {
        /// What a read sees.
        current: Vec<u8>,
        /// The contents as of the last sync.
        durable: Vec<u8>,
        /// Operations since the last sync, oldest first.
        unsynced: Vec<Op>,
    }

    impl SimFile {
        fn record(&mut self, op: Op) {
            apply(&mut self.current, &op);
            self.unsynced.push(op);
        }
    }

    fn apply(bytes: &mut Vec<u8>, op: &Op) {
        match op {
            Op::Write(offset, data) => {
                let (start, end) = (*offset as usize, *offset as usize + data.len());
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[start..end].copy_from_slice(data);
            }
            Op::SetLen(len) => bytes.resize(*len as usize, 0),
        }
    }

    #[derive(Default)]
    struct State {
        files: Vec<SimFile>,
        /// Operations left before the crash; `None` = never crash.
        ops_left: Option<usize>,
        /// Whether only that one operation fails, and the rest succeed again.
        transient: bool,
        /// Operations done so far.
        ops: usize,
    }

    #[derive(Default)]
    pub struct Sim(Mutex<State>);

    impl Sim {
        /// A new set of empty files.
        pub fn new(files: usize) -> Arc<Sim> {
            let sim = Sim::default();
            sim.0.lock().unwrap().files = vec![SimFile::default(); files];
            Arc::new(sim)
        }

        pub fn file(self: &Arc<Self>, index: usize) -> Box<dyn Medium> {
            Box::new(SimMedium { sim: self.clone(), index })
        }

        /// Crashes after `ops` more operations.
        pub fn crash_after(&self, ops: usize) {
            let mut state = self.0.lock().unwrap();
            (state.ops_left, state.transient) = (Some(ops), false);
        }

        /// Fails the operation after `ops` more, as an I/O error would, and no other.
        pub fn fail_once_after(&self, ops: usize) {
            let mut state = self.0.lock().unwrap();
            (state.ops_left, state.transient) = (Some(ops), true);
        }

        /// Cancels a pending crash or failure.
        pub fn heal(&self) {
            self.0.lock().unwrap().ops_left = None;
        }

        pub fn ops(&self) -> usize {
            self.0.lock().unwrap().ops
        }

        /// The files a restart finds: the first `kept` unsynced operations of
        /// each file reached the disk (all of them if there are fewer).
        pub fn crashed(&self, kept: usize) -> Arc<Sim> {
            let state = self.0.lock().unwrap();
            let files = state
                .files
                .iter()
                .map(|file| {
                    let mut bytes = file.durable.clone();
                    file.unsynced.iter().take(kept).for_each(|op| apply(&mut bytes, op));
                    SimFile { current: bytes.clone(), durable: bytes, unsynced: Vec::new() }
                })
                .collect();
            Arc::new(Sim(Mutex::new(State { files, ..State::default() })))
        }

        fn op<T>(&self, index: usize, f: impl FnOnce(&mut SimFile) -> T) -> Result<T> {
            let mut state = self.0.lock().unwrap();
            match state.ops_left {
                Some(0) if state.transient => {
                    state.ops_left = None;
                    return err("simulated I/O error");
                }
                Some(0) => return err("simulated crash"),
                Some(n) => state.ops_left = Some(n - 1),
                None => {}
            }
            state.ops += 1;
            Ok(f(&mut state.files[index]))
        }
    }

    struct SimMedium {
        sim: Arc<Sim>,
        index: usize,
    }

    impl Medium for SimMedium {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let state = self.sim.0.lock().unwrap();
            match state.files[self.index].current.get(offset as usize..offset as usize + buf.len()) {
                Some(data) => buf.copy_from_slice(data),
                None => return err(format!("read past the end at {offset}")),
            }
            Ok(())
        }

        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            self.sim.op(self.index, |file| file.record(Op::Write(offset, buf.to_vec())))
        }

        fn size(&self) -> Result<u64> {
            Ok(self.sim.0.lock().unwrap().files[self.index].current.len() as u64)
        }

        fn set_len(&self, len: u64) -> Result<()> {
            self.sim.op(self.index, |file| file.record(Op::SetLen(len)))
        }

        fn sync(&self) -> Result<()> {
            self.sim.op(self.index, |file| {
                file.durable = file.current.clone();
                file.unsynced.clear();
            })
        }
    }
}
