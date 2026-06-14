use std::cell::RefCell;
use std::hash::Hasher;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

static DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();
static RUN_TS: OnceLock<String> = OnceLock::new();
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn is_debug_enabled() -> bool {
    *DEBUG_ENABLED.get_or_init(|| std::env::var("HEPH_DEBUG_HASH").is_ok())
}

fn run_ts() -> &'static str {
    RUN_TS.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().to_string())
            .unwrap_or_else(|_| "0".to_string())
    })
}

fn open_debug_file(target: &str) -> Option<std::io::BufWriter<std::fs::File>> {
    let safe = target.trim_start_matches('/').replace(['/', ':'], "-");
    let dir = std::path::Path::new("/tmp/debughash")
        .join(run_ts())
        .join(&safe);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    match std::fs::File::create(dir.join(format!("{}.txt", seq)).as_path()) {
        Ok(f) => Some(std::io::BufWriter::new(f)),
        Err(_) => None,
    }
}

/// Wraps any `Hasher` and, when `HEPH_DEBUG_HASH` is set, logs every write call
/// as a line to `/tmp/debughash/<run_ts>/<target>/<seq>`. Zero overhead when the
/// env var is absent (single OnceLock check at construction, no file I/O).
pub struct DebugHasher<H: Hasher> {
    inner: H,
    file: Option<RefCell<std::io::BufWriter<std::fs::File>>>,
}

impl<H: Hasher> DebugHasher<H> {
    pub fn new(inner: H, target: impl FnOnce() -> String) -> Self {
        let file = if is_debug_enabled() {
            open_debug_file(&target()).map(RefCell::new)
        } else {
            None
        };
        Self { inner, file }
    }

    fn log_bytes(&mut self, bytes: &[u8]) {
        let failed = if let Some(ref cell) = self.file {
            let mut f = cell.borrow_mut();
            let mut hex = String::with_capacity(bytes.len() * 2);
            for b in bytes {
                hex.push_str(&format!("{b:02x}"));
            }
            writeln!(&mut *f, "{hex}").is_err()
        } else {
            false
        };
        if failed {
            self.file = None;
        }
    }

    fn log_val(&mut self, prefix: &str, val: impl std::fmt::Display) {
        let failed = if let Some(ref cell) = self.file {
            let mut f = cell.borrow_mut();
            writeln!(&mut *f, "{prefix}:{val}").is_err()
        } else {
            false
        };
        if failed {
            self.file = None;
        }
    }
}

impl<H: Hasher> Hasher for DebugHasher<H> {
    fn finish(&self) -> u64 {
        let result = self.inner.finish();
        if let Some(ref cell) = self.file {
            let mut f = cell.borrow_mut();
            drop(write!(&mut *f, "\n\n{result:x}\n"));
        }
        result
    }

    fn write(&mut self, bytes: &[u8]) {
        self.inner.write(bytes);
        self.log_bytes(bytes);
    }

    fn write_u8(&mut self, i: u8) {
        self.inner.write_u8(i);
        self.log_val("u8", i);
    }

    fn write_u16(&mut self, i: u16) {
        self.inner.write_u16(i);
        self.log_val("u16", i);
    }

    fn write_u32(&mut self, i: u32) {
        self.inner.write_u32(i);
        self.log_val("u32", i);
    }

    fn write_u64(&mut self, i: u64) {
        self.inner.write_u64(i);
        self.log_val("u64", i);
    }

    fn write_u128(&mut self, i: u128) {
        self.inner.write_u128(i);
        self.log_val("u128", i);
    }

    fn write_usize(&mut self, i: usize) {
        self.inner.write_usize(i);
        self.log_val("usize", i);
    }

    fn write_i8(&mut self, i: i8) {
        self.inner.write_i8(i);
        self.log_val("i8", i);
    }

    fn write_i16(&mut self, i: i16) {
        self.inner.write_i16(i);
        self.log_val("i16", i);
    }

    fn write_i32(&mut self, i: i32) {
        self.inner.write_i32(i);
        self.log_val("i32", i);
    }

    fn write_i64(&mut self, i: i64) {
        self.inner.write_i64(i);
        self.log_val("i64", i);
    }

    fn write_i128(&mut self, i: i128) {
        self.inner.write_i128(i);
        self.log_val("i128", i);
    }

    fn write_isize(&mut self, i: isize) {
        self.inner.write_isize(i);
        self.log_val("isize", i);
    }
}
