//! Compile-time stub used when FUSE support is unavailable (non-linux or
//! the `fuse-sandbox` feature is off). All entry points return errors so
//! the bridge transparently takes the copy path.

use heph_core::hartifactcontent::ReadSeek;
use heph_core::hartifactcontent::tar_index::TarIndex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type LayerOpener = Box<dyn Fn() -> anyhow::Result<Box<dyn ReadSeek + Send>> + Send + Sync>;

pub struct Layer {
    pub index: TarIndex,
    pub open: LayerOpener,
}

impl Layer {
    pub fn new(index: TarIndex, open: LayerOpener) -> Self {
        Self { index, open }
    }
}

pub struct LayeredFs {
    pub upper_root: PathBuf,
}

impl LayeredFs {
    pub fn new(_layers: Vec<Layer>, upper_root: PathBuf) -> Self {
        Self { upper_root }
    }

    pub fn new_empty(upper_root: PathBuf) -> Self {
        Self { upper_root }
    }

    pub fn register_slot(self: &Arc<Self>, prefix: PathBuf, _layers: Vec<Layer>) -> SlotGuard {
        SlotGuard {
            _fs: Arc::clone(self),
            _prefix: prefix,
        }
    }
}

pub struct SlotGuard {
    _fs: Arc<LayeredFs>,
    _prefix: PathBuf,
}

pub struct Mount;

#[expect(
    clippy::self_named_constructors,
    reason = "verb matches the operation: mounting yields a Mount handle"
)]
impl Mount {
    pub fn mount(_mountpoint: &Path, _fs: Arc<LayeredFs>) -> anyhow::Result<Self> {
        anyhow::bail!("FUSE sandbox is unavailable on this build")
    }
}
