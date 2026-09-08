#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStat {
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedFileHash {
    pub stat: FileStat,
    pub content_hash: String,
    pub hashed_at_ns: i64,
}

impl CachedFileHash {
    pub fn reusable(&self, stat: &FileStat, now_ns: i64) -> bool {
        // A whole second guards filesystems whose timestamps have coarse resolution.
        let write_second = self.hashed_at_ns.div_euclid(1_000_000_000);
        self.stat == *stat
            && self.hashed_at_ns <= now_ns
            && stat.mtime_ns.div_euclid(1_000_000_000) < write_second
            && stat.ctime_ns.div_euclid(1_000_000_000) < write_second
    }
}
