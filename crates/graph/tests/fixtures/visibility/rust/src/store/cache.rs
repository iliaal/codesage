use super::open_private;

pub fn cached() -> u8 {
    open_private() + 1
}
