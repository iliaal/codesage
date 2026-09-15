use crate::store::{Backend, Store, crate_only, open_public};

pub fn handle() -> u8 {
    open_private() + crate_only() + open_public()
}

pub fn flush_all(store: &Store) {
    Store::flush(store);
}
