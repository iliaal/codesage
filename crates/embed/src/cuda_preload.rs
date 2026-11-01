//! Derive the CUDA/cuDNN preload set from the ONNX Runtime CUDA provider's
//! `DT_NEEDED` entries and resolve each soname against the discovered NVIDIA
//! library directories.
//!
//! Directory rule: the provider's CUDA major is the `N` of the `libcudart.so.N`
//! it needs. Among directories holding a soname, a bundle directory with a
//! `cu<N>` path component ranks first, a directory with no `cu<digits>`
//! component (pip's per-library `nvidia/<lib>/lib`) second, and a bundle for
//! another CUDA major last: pip's `nvidia/cu13/lib` ships its own
//! `libcurand.so.10`, which a CUDA 12 provider must not pick up. Ties go to the
//! earliest directory in discovery order, which is deterministic because
//! discovery sorts directory entries.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

pub(crate) const CUDA_PROVIDER_FILE: &str = "libonnxruntime_providers_cuda.so";

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
const EHDR_LEN: usize = 64;
const PHDR_LEN: usize = 56;
const DYN_LEN: usize = 16;
const MAX_PHNUM: usize = 4096;
const MAX_DYNAMIC_BYTES: u64 = 1 << 20;
const MAX_SONAME_LEN: u64 = 4096;

/// Dependencies load before their dependents: `dlopen` of an absolute path
/// still resolves the library's own `DT_NEEDED` by soname.
const LOAD_ORDER: &[&str] = &[
    "libcudart.so",
    "libnvJitLink.so",
    "libcublasLt.so",
    "libcublas.so",
    "libnvrtc.so",
    "libcurand.so",
    "libcufft.so",
    "libcusparse.so",
    "libcusolver.so",
    "libcudnn.so",
];

pub(crate) trait ReadAt {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
}

impl ReadAt for [u8] {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let start = usize::try_from(offset).context("ELF offset overflows usize")?;
        let end = start.checked_add(len).context("ELF range overflows")?;
        self.get(start..end).map(<[u8]>::to_vec).with_context(|| {
            format!(
                "ELF range {start}..{end} exceeds file length {}",
                self.len()
            )
        })
    }
}

#[cfg(unix)]
impl ReadAt for std::fs::File {
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let mut buf = vec![0; len];
        self.read_exact_at(&mut buf, offset)
            .with_context(|| format!("reading {len} bytes at offset {offset}"))?;
        Ok(buf)
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4-byte slice"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8-byte slice"))
}

struct Load {
    offset: u64,
    vaddr: u64,
    filesz: u64,
}

/// `DT_NEEDED` sonames of an ELF64 little-endian shared object, in file order.
pub(crate) fn elf_needed<R: ReadAt + ?Sized>(src: &R) -> Result<Vec<String>> {
    let ehdr = src.read_at(0, EHDR_LEN).context("reading ELF header")?;
    ensure!(ehdr[..4] == *b"\x7fELF", "not an ELF file");
    ensure!(ehdr[4] == 2, "not an ELF64 file");
    ensure!(ehdr[5] == 1, "not a little-endian ELF file");
    let phoff = u64_at(&ehdr, 0x20);
    let phentsize = usize::from(u16_at(&ehdr, 0x36));
    let phnum = usize::from(u16_at(&ehdr, 0x38));
    ensure!(
        phentsize >= PHDR_LEN,
        "program header entry too small: {phentsize}"
    );
    ensure!(phnum <= MAX_PHNUM, "too many program headers: {phnum}");
    let phdrs = src
        .read_at(phoff, phentsize * phnum)
        .context("reading program headers")?;

    let mut loads = Vec::new();
    let mut dynamic = None;
    for ph in phdrs.chunks_exact(phentsize) {
        let p_type = u32_at(ph, 0);
        let offset = u64_at(ph, 8);
        let vaddr = u64_at(ph, 16);
        let filesz = u64_at(ph, 32);
        match p_type {
            PT_LOAD => loads.push(Load {
                offset,
                vaddr,
                filesz,
            }),
            PT_DYNAMIC => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic.context("no PT_DYNAMIC segment")?;
    ensure!(
        dyn_size <= MAX_DYNAMIC_BYTES,
        "PT_DYNAMIC segment too large: {dyn_size} bytes"
    );
    let dyn_bytes = src
        .read_at(dyn_off, usize::try_from(dyn_size)?)
        .context("reading PT_DYNAMIC segment")?;

    let mut needed = Vec::new();
    let mut strtab = None;
    let mut strsz = None;
    for entry in dyn_bytes.chunks_exact(DYN_LEN) {
        let tag = u64_at(entry, 0);
        let val = u64_at(entry, 8);
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed.push(val),
            DT_STRTAB => strtab = Some(val),
            DT_STRSZ => strsz = Some(val),
            _ => {}
        }
    }
    if needed.is_empty() {
        return Ok(Vec::new());
    }
    let strtab = strtab.context("DT_NEEDED present without DT_STRTAB")?;
    let strsz = strsz.context("DT_NEEDED present without DT_STRSZ")?;
    let load = loads
        .iter()
        .find(|l| strtab >= l.vaddr && strtab - l.vaddr < l.filesz)
        .with_context(|| format!("DT_STRTAB {strtab:#x} is not inside any PT_LOAD segment"))?;
    let in_segment = load.filesz - (strtab - load.vaddr);
    let strtab_len = strsz.min(in_segment);
    let strtab_off = load
        .offset
        .checked_add(strtab - load.vaddr)
        .context("DT_STRTAB file offset overflows")?;

    needed
        .into_iter()
        .map(|name_off| {
            ensure!(
                name_off < strtab_len,
                "DT_NEEDED string offset {name_off} outside string table of {strtab_len} bytes"
            );
            let window = (strtab_len - name_off).min(MAX_SONAME_LEN);
            let bytes = src
                .read_at(strtab_off + name_off, usize::try_from(window)?)
                .context("reading DT_NEEDED string")?;
            let Some(nul) = bytes.iter().position(|&b| b == 0) else {
                bail!("unterminated DT_NEEDED string at offset {name_off}");
            };
            String::from_utf8(bytes[..nul].to_vec()).context("DT_NEEDED string is not UTF-8")
        })
        .collect()
}

#[cfg(unix)]
pub(crate) fn file_needed(path: &Path) -> Result<Vec<String>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    elf_needed(&file).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn file_needed(path: &Path) -> Result<Vec<String>> {
    bail!(
        "ELF parsing is unsupported on this platform: {}",
        path.display()
    )
}

fn load_rank(soname: &str) -> usize {
    LOAD_ORDER
        .iter()
        .position(|prefix| soname.starts_with(prefix))
        .unwrap_or(LOAD_ORDER.len())
}

/// The CUDA/cuDNN sonames from a `DT_NEEDED` list, in dependency load order.
/// The driver (`libcuda.so`) is excluded: it comes from the system, never pip.
pub(crate) fn cuda_sonames(needed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = needed
        .iter()
        .filter(|n| load_rank(n) < LOAD_ORDER.len())
        .cloned()
        .collect();
    out.sort_by_key(|n| load_rank(n));
    out.dedup();
    out
}

fn soname_major(soname: &str, stem: &str) -> Option<u32> {
    soname.strip_prefix(stem)?.split('.').next()?.parse().ok()
}

pub(crate) fn cuda_major<S: AsRef<str>>(sonames: &[S]) -> Option<u32> {
    sonames
        .iter()
        .find_map(|s| soname_major(s.as_ref(), "libcudart.so."))
}

fn bundle_major(dir: &Path) -> Option<u32> {
    dir.components().find_map(|c| {
        let s = c.as_os_str().to_str()?;
        let digits = s.strip_prefix("cu")?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    })
}

/// The directory to load `soname` from; see the module docs for the rule.
pub(crate) fn resolve_dir<'a>(
    soname: &str,
    cuda_major: Option<u32>,
    lib_dirs: &'a [PathBuf],
) -> Option<&'a Path> {
    lib_dirs
        .iter()
        .enumerate()
        .filter(|(_, dir)| dir.join(soname).is_file())
        .min_by_key(|(index, dir)| {
            let rank = match (cuda_major, bundle_major(dir)) {
                (Some(want), Some(have)) if want == have => 0,
                (_, None) | (None, Some(_)) => 1,
                (Some(_), Some(_)) => 2,
            };
            (rank, *index)
        })
        .map(|(_, dir)| dir.as_path())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PlanSource {
    Provider(PathBuf),
    Fallback(String),
}

#[derive(Debug)]
pub(crate) struct PreloadPlan {
    pub(crate) source: PlanSource,
    pub(crate) sonames: Vec<String>,
}

/// Sonames from `provider` when it parses and lists any CUDA library;
/// otherwise `fallback`, with the reason recorded in the plan source.
pub(crate) fn plan(provider: Option<&Path>, fallback: &[&str]) -> PreloadPlan {
    let derived = match provider {
        None => Err("ONNX Runtime library location unknown".to_string()),
        Some(path) if !path.is_file() => Err(format!("{} not found", path.display())),
        Some(path) => match file_needed(path) {
            Ok(needed) => {
                let sonames = cuda_sonames(&needed);
                if sonames.is_empty() {
                    Err(format!("{} lists no CUDA libraries", path.display()))
                } else {
                    Ok((path.to_path_buf(), sonames))
                }
            }
            Err(e) => Err(format!("{e:#}")),
        },
    };
    match derived {
        Ok((path, sonames)) => PreloadPlan {
            source: PlanSource::Provider(path),
            sonames,
        },
        Err(reason) => PreloadPlan {
            source: PlanSource::Fallback(reason),
            sonames: fallback.iter().map(|s| (*s).to_string()).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRTAB_VADDR: u64 = 0x2000;
    const LOAD_VADDR: u64 = 0x1000;

    /// A minimal ELF64 LE shared object assembled field by field, independent
    /// of the parser: one PT_LOAD mapping file offset 0 at vaddr 0x1000, one
    /// PT_DYNAMIC, and a string table at file offset 0x1000.
    fn hand_built_elf(needed: &[&str]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut name_offsets = Vec::new();
        for name in needed {
            name_offsets.push(strtab.len() as u64);
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
        }
        let mut dynamic = Vec::new();
        let mut push_dyn = |tag: u64, val: u64| {
            dynamic.extend_from_slice(&tag.to_le_bytes());
            dynamic.extend_from_slice(&val.to_le_bytes());
        };
        push_dyn(DT_STRSZ, strtab.len() as u64);
        for off in &name_offsets {
            push_dyn(DT_NEEDED, *off);
        }
        push_dyn(DT_STRTAB, STRTAB_VADDR);
        push_dyn(DT_NULL, 0);

        let phoff = 64u64;
        let dyn_off = 0x200u64;
        let strtab_off = STRTAB_VADDR - LOAD_VADDR;
        let total = strtab_off as usize + strtab.len();
        let mut f = vec![0u8; total];
        f[..4].copy_from_slice(b"\x7fELF");
        f[4] = 2;
        f[5] = 1;
        f[6] = 1;
        f[0x10..0x12].copy_from_slice(&3u16.to_le_bytes());
        f[0x12..0x14].copy_from_slice(&62u16.to_le_bytes());
        f[0x20..0x28].copy_from_slice(&phoff.to_le_bytes());
        f[0x34..0x36].copy_from_slice(&64u16.to_le_bytes());
        f[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        f[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());

        let mut put_phdr = |index: usize, p_type: u32, offset: u64, vaddr: u64, size: u64| {
            let at = phoff as usize + index * 56;
            f[at..at + 4].copy_from_slice(&p_type.to_le_bytes());
            f[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            f[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            f[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
            f[at + 40..at + 48].copy_from_slice(&size.to_le_bytes());
        };
        put_phdr(0, PT_LOAD, 0, LOAD_VADDR, total as u64);
        put_phdr(
            1,
            PT_DYNAMIC,
            dyn_off,
            LOAD_VADDR + dyn_off,
            dynamic.len() as u64,
        );
        f[dyn_off as usize..dyn_off as usize + dynamic.len()].copy_from_slice(&dynamic);
        f[strtab_off as usize..].copy_from_slice(&strtab);
        f
    }

    #[test]
    fn parses_needed_from_hand_built_elf() {
        let names = ["libcublasLt.so.12", "libcudart.so.12", "libc.so.6"];
        let elf = hand_built_elf(&names);
        assert_eq!(elf_needed(elf.as_slice()).unwrap(), names);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_needed_of_own_executable_like_readelf() {
        let exe = std::fs::read_link("/proc/self/exe").unwrap();
        let out = std::process::Command::new("readelf")
            .arg("-dW")
            .arg(&exe)
            .output()
            .expect("readelf (binutils) is required for this test");
        assert!(out.status.success(), "readelf failed");
        let expected: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter(|l| l.contains("(NEEDED)"))
            .filter_map(|l| {
                let start = l.find('[')? + 1;
                let end = l.rfind(']')?;
                Some(l[start..end].to_string())
            })
            .collect();
        assert!(!expected.is_empty(), "readelf listed no NEEDED entries");
        assert_eq!(file_needed(&exe).unwrap(), expected);
    }

    #[test]
    fn malformed_elf_is_an_error_not_a_panic() {
        let elf = hand_built_elf(&["libcudart.so.12"]);
        for len in 0..elf.len() {
            let _ = elf_needed(&elf[..len]);
        }
        assert!(elf_needed(&elf[..40]).is_err());
        assert!(elf_needed(&elf[..0x208]).is_err());
        assert!(elf_needed(&elf[..elf.len() - 4]).is_err());
        assert!(
            elf_needed(
                b"garbage, not an ELF file at all; padding padding padding padding".as_slice()
            )
            .is_err()
        );

        let mut class32 = elf.clone();
        class32[4] = 1;
        assert!(elf_needed(class32.as_slice()).is_err());

        let mut wild_phoff = elf.clone();
        wild_phoff[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(elf_needed(wild_phoff.as_slice()).is_err());

        let mut wild_strtab = elf.clone();
        let strtab_tag = wild_strtab
            .windows(8)
            .position(|w| w == DT_STRTAB.to_le_bytes())
            .unwrap();
        wild_strtab[strtab_tag + 8..strtab_tag + 16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(elf_needed(wild_strtab.as_slice()).is_err());
    }

    #[test]
    fn garbage_provider_falls_back_to_static_list() {
        let dir = tempfile::tempdir().unwrap();
        let provider = dir.path().join(CUDA_PROVIDER_FILE);
        std::fs::write(&provider, b"\x7fELF\x02\x01truncated").unwrap();
        let fallback = ["libcudart.so.12", "libcudnn.so.9"];
        let p = plan(Some(&provider), &fallback);
        assert!(
            matches!(p.source, PlanSource::Fallback(_)),
            "{:?}",
            p.source
        );
        assert_eq!(p.sonames, fallback);

        let missing = plan(Some(&dir.path().join("absent.so")), &fallback);
        assert!(matches!(missing.source, PlanSource::Fallback(_)));
        assert_eq!(missing.sonames, fallback);
    }

    #[test]
    fn provider_plan_keeps_only_cuda_sonames_in_load_order() {
        let dir = tempfile::tempdir().unwrap();
        let provider = dir.path().join(CUDA_PROVIDER_FILE);
        std::fs::write(
            &provider,
            hand_built_elf(&[
                "libcublasLt.so.13",
                "libcublas.so.13",
                "libcuda.so.1",
                "libcudnn.so.9",
                "libcudart.so.13",
                "libstdc++.so.6",
            ]),
        )
        .unwrap();
        let p = plan(Some(&provider), &["libcudart.so.12"]);
        assert_eq!(p.source, PlanSource::Provider(provider));
        assert_eq!(
            p.sonames,
            [
                "libcudart.so.13",
                "libcublasLt.so.13",
                "libcublas.so.13",
                "libcudnn.so.9"
            ]
        );
        assert_eq!(cuda_major(&p.sonames), Some(13));
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn curand_resolves_to_the_provider_cuda_major_regardless_of_order() {
        let root = tempfile::tempdir().unwrap();
        let nvidia = root.path().join("nvidia");
        touch(&nvidia.join("cu13/lib/libcurand.so.10"));
        touch(&nvidia.join("cu13/lib/libcudart.so.13"));
        touch(&nvidia.join("curand/lib/libcurand.so.10"));
        touch(&nvidia.join("cuda_runtime/lib/libcudart.so.12"));
        let cu13 = nvidia.join("cu13/lib");
        let curand = nvidia.join("curand/lib");

        let discovered = crate::model::collect_nvidia_lib_dirs(std::slice::from_ref(&nvidia));
        assert_eq!(
            discovered.iter().position(|d| *d == cu13),
            Some(0),
            "discovery is sorted, so the cu13 bundle comes first: {discovered:?}"
        );
        let mut reversed = discovered.clone();
        reversed.reverse();

        for dirs in [&discovered, &reversed] {
            assert_eq!(
                resolve_dir("libcurand.so.10", Some(12), dirs),
                Some(curand.as_path())
            );
            assert_eq!(
                resolve_dir("libcurand.so.10", Some(13), dirs),
                Some(cu13.as_path())
            );
            assert_eq!(
                resolve_dir("libcudart.so.12", Some(12), dirs),
                Some(nvidia.join("cuda_runtime/lib").as_path())
            );
            assert_eq!(resolve_dir("libcublas.so.12", Some(12), dirs), None);
        }
    }
}
