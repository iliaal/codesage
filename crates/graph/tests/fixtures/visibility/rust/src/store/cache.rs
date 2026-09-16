use super::open_private;

pub fn cached() -> u8 {
    open_private() + 1
}

pub(super) fn evict() -> u8 {
    0
}
