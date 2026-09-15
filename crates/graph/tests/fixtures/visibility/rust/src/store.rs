mod cache;

use self::cache::evict;

pub struct Store;

pub trait Backend {
    fn flush(&self);
}

impl Backend for Store {
    fn flush(&self) {}
}

fn open_private() -> u8 {
    1
}

pub(crate) fn crate_only() -> u8 {
    2
}

pub fn open_public() -> u8 {
    open_private() + cache::cached() + evict()
}
