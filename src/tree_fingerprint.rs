//! Persisted fingerprint of a managed install tree.
//!
//! `cleanup::ManagedRootIdentity` records this fingerprint in method-transition
//! and prune journals, and a later process (possibly a later Shdeps release,
//! built by another Rust toolchain, possibly after a reboot) recomputes it
//! before deleting the tree. Two properties therefore matter beyond hashing
//! every entry:
//!
//! - **Stable algorithm.** Earlier releases hashed with
//!   `std::collections::hash_map::DefaultHasher`, whose algorithm std does
//!   not promise across releases. This module pins the algorithm those
//!   releases actually ran (SipHash-1-3 with zero keys, fed the same bytes
//!   the std `Hash` impls fed it) so every fingerprint already on disk still
//!   verifies, and older releases can still verify the ones written now.
//!   Changing the value would also change no format: older readers would
//!   merely refuse to clean up, so the value itself is the compatibility
//!   contract.
//! - **Reboot-safe devices.** Each entry's `st_dev` is part of the value, and
//!   a remount can renumber it (see `persisted_identity`). Once the tree's
//!   root has been proven to be the recorded object, entries on the root's
//!   live device are hashed as the root's recorded device, which reproduces
//!   the recorded value exactly when nothing else changed. Entries on any
//!   other device (a mount inside the tree) are hashed as they are, so such a
//!   tree verifies only while that inner device keeps its number.

use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use crate::Result;

/// The device number a remount gave a tree's root, and the one recorded.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Renumbering {
    /// The root's `st_dev` now.
    pub(crate) live: u64,
    /// The root's `st_dev` in the journal being verified.
    pub(crate) recorded: u64,
}

impl Renumbering {
    /// No renumbering: fingerprint the tree as it is, on `device`.
    pub(crate) fn none(device: u64) -> Self {
        Self {
            live: device,
            recorded: device,
        }
    }
}

/// Fingerprints every entry below `root`: its relative name, device, inode,
/// mode, size, modification and change times, symlink target, and the
/// entries of each subdirectory, in name order.
pub(crate) fn of(root: &Path, devices: Renumbering) -> Result<u64> {
    let mut hasher = SipHasher13::default();
    visit(root, root, devices, &mut hasher)?;
    Ok(hasher.finish())
}

// The exact byte stream the legacy `Hash` impls produced: a byte slice is its
// `usize` length then its bytes, and every integer its native-endian bytes
// (`i64` as `u64`). Native order is fine: a fingerprint is only ever
// recomputed on the machine that recorded it.
fn visit(root: &Path, dir: &Path, devices: Renumbering, hasher: &mut SipHasher13) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "managed-root entry escaped its captured root",
            )
        })?;
        hasher.write_bytes(relative.as_os_str().as_bytes());
        let metadata = fs::symlink_metadata(&path)?;
        let device = if metadata.dev() == devices.live {
            devices.recorded
        } else {
            metadata.dev()
        };
        hasher.write(&device.to_ne_bytes());
        hasher.write(&metadata.ino().to_ne_bytes());
        hasher.write(&metadata.mode().to_ne_bytes());
        hasher.write(&metadata.size().to_ne_bytes());
        hasher.write(&metadata.mtime().to_ne_bytes());
        hasher.write(&metadata.mtime_nsec().to_ne_bytes());
        hasher.write(&metadata.ctime().to_ne_bytes());
        hasher.write(&metadata.ctime_nsec().to_ne_bytes());
        if metadata.file_type().is_symlink() {
            hasher.write_bytes(fs::read_link(&path)?.as_os_str().as_bytes());
        } else if metadata.file_type().is_dir() {
            visit(root, &path, devices, hasher)?;
        }
    }
    Ok(())
}

/// SipHash-1-3 keyed with zeros: what `DefaultHasher::new()` has computed
/// since Rust 1.13, restated so the value no longer depends on std.
#[derive(Debug, Clone)]
struct SipHasher13 {
    v: [u64; 4],
    tail: [u8; 8],
    tail_len: usize,
    length: usize,
}

impl Default for SipHasher13 {
    fn default() -> Self {
        // The SipHash initialization constants, each XORed with a zero key.
        Self {
            v: [
                0x736f_6d65_7073_6575,
                0x646f_7261_6e64_6f6d,
                0x6c79_6765_6e65_7261,
                0x7465_6462_7974_6573,
            ],
            tail: [0; 8],
            tail_len: 0,
            length: 0,
        }
    }
}

impl SipHasher13 {
    /// Feeds a length-prefixed byte slice, as `<[u8] as Hash>::hash` does.
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write(&bytes.len().to_ne_bytes());
        self.write(bytes);
    }

    /// Feeds raw bytes; the result depends only on their concatenation.
    fn write(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len());
        while !bytes.is_empty() {
            let take = (8 - self.tail_len).min(bytes.len());
            self.tail[self.tail_len..self.tail_len + take].copy_from_slice(&bytes[..take]);
            self.tail_len += take;
            bytes = &bytes[take..];
            if self.tail_len == 8 {
                self.compress(u64::from_le_bytes(self.tail));
                self.tail_len = 0;
            }
        }
    }

    fn finish(&self) -> u64 {
        let mut state = self.clone();
        let mut last = [0; 8];
        last[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
        // The final block carries the low byte of the total length on top.
        let block = u64::from_le_bytes(last) | ((self.length as u64 & 0xff) << 56);
        state.compress(block);
        state.v[2] ^= 0xff;
        for _ in 0..3 {
            state.round();
        }
        state.v[0] ^ state.v[1] ^ state.v[2] ^ state.v[3]
    }

    /// One message block through the single compression round of SipHash-1-3.
    fn compress(&mut self, block: u64) {
        self.v[3] ^= block;
        self.round();
        self.v[0] ^= block;
    }

    fn round(&mut self) {
        let [v0, v1, v2, v3] = &mut self.v;
        *v0 = v0.wrapping_add(*v1);
        *v1 = v1.rotate_left(13);
        *v1 ^= *v0;
        *v0 = v0.rotate_left(32);
        *v2 = v2.wrapping_add(*v3);
        *v3 = v3.rotate_left(16);
        *v3 ^= *v2;
        *v0 = v0.wrapping_add(*v3);
        *v3 = v3.rotate_left(21);
        *v3 ^= *v0;
        *v2 = v2.wrapping_add(*v1);
        *v1 = v1.rotate_left(17);
        *v1 ^= *v2;
        *v2 = v2.rotate_left(32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    /// The fingerprint every earlier release computed, verbatim, except that
    /// `device_as` may stand in for each entry's device: the value a release
    /// would have recorded before a remount renumbered those devices.
    fn legacy(root: &Path, device_as: impl Fn(u64) -> u64 + Copy) -> u64 {
        fn visit(
            root: &Path,
            dir: &Path,
            device_as: impl Fn(u64) -> u64 + Copy,
            hasher: &mut impl Hasher,
        ) {
            let mut entries = fs::read_dir(dir)
                .unwrap()
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap();
                relative.as_os_str().as_bytes().hash(hasher);
                let metadata = fs::symlink_metadata(&path).unwrap();
                device_as(metadata.dev()).hash(hasher);
                metadata.ino().hash(hasher);
                metadata.mode().hash(hasher);
                metadata.size().hash(hasher);
                metadata.mtime().hash(hasher);
                metadata.mtime_nsec().hash(hasher);
                metadata.ctime().hash(hasher);
                metadata.ctime_nsec().hash(hasher);
                if metadata.file_type().is_symlink() {
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .hash(hasher);
                } else if metadata.file_type().is_dir() {
                    visit(root, &path, device_as, hasher);
                }
            }
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        visit(root, root, device_as, &mut hasher);
        hasher.finish()
    }

    fn tree(name: &str) -> std::path::PathBuf {
        let root = crate::test_support::temp_dir(name);
        fs::create_dir_all(root.join("bin/nested")).unwrap();
        fs::write(root.join("bin/tool"), "tool\n").unwrap();
        fs::write(root.join("bin/nested/data"), "").unwrap();
        std::os::unix::fs::symlink("bin/tool", root.join("link")).unwrap();
        root
    }

    fn device(path: &Path) -> u64 {
        fs::symlink_metadata(path).unwrap().dev()
    }

    #[test]
    fn hasher_reproduces_the_values_released_binaries_recorded() {
        // Pinned outputs of `DefaultHasher::new()` (Rust 1.85 through 1.98)
        // for the write sequences a fingerprint issues: these are the values
        // already on disk, so they must never change.
        let digest = |writes: &dyn Fn(&mut SipHasher13)| {
            let mut hasher = SipHasher13::default();
            writes(&mut hasher);
            hasher.finish()
        };
        assert_eq!(digest(&|_| {}), 0xd1fb_a762_150c_532c);
        assert_eq!(digest(&|h| h.write(b"abcdefg")), 0x6db1_2aae_9070_f506);
        assert_eq!(digest(&|h| h.write(b"abcdefgh")), 0x3f7b_849c_0b8e_35ea);
        // Integers enter in native byte order; every release target is
        // little-endian.
        if cfg!(target_endian = "little") {
            assert_eq!(
                digest(&|h| {
                    h.write_bytes(b"bin/tool");
                    h.write(&16_777_229_u64.to_ne_bytes());
                    h.write(&0o100_644_u32.to_ne_bytes());
                    h.write(&(-1_i64).to_ne_bytes());
                }),
                0xaad2_d6d8_91c0_c750
            );
        }
    }

    #[test]
    fn hasher_matches_default_hasher_for_any_chunking() {
        let expected = |bytes: &[u8]| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(bytes);
            hasher.finish()
        };
        let bytes = (0..=255_u8).collect::<Vec<_>>();
        for len in 0..bytes.len() {
            let mut ours = SipHasher13::default();
            let (head, tail) = bytes[..len].split_at(len / 3);
            ours.write(head);
            ours.write(tail);
            assert_eq!(ours.finish(), expected(&bytes[..len]), "length {len}");
        }
    }

    #[test]
    fn fingerprint_equals_the_legacy_value() {
        let root = tree("tree-fingerprint-legacy");
        assert_eq!(
            of(&root, Renumbering::none(device(&root))).unwrap(),
            legacy(&root, |device| device)
        );
    }

    #[test]
    fn a_renumbered_root_device_reproduces_the_recorded_value() {
        // Recorded before a reboot moved the tree's device from `live + 1`
        // to `live`; an entry on another device keeps its own number.
        let root = tree("tree-fingerprint-renumbered");
        let live = device(&root);
        let recorded = legacy(
            &root,
            |device| if device == live { live + 1 } else { device },
        );
        let renumbered = Renumbering {
            live,
            recorded: live + 1,
        };
        assert_eq!(of(&root, renumbered).unwrap(), recorded);
        assert_ne!(of(&root, Renumbering::none(live)).unwrap(), recorded);
    }

    #[test]
    fn any_entry_change_moves_the_fingerprint() {
        let root = tree("tree-fingerprint-changes");
        let devices = Renumbering::none(device(&root));
        let original = of(&root, devices).unwrap();
        fs::write(root.join("bin/nested/data"), "x").unwrap();
        assert_ne!(of(&root, devices).unwrap(), original);

        let edited = of(&root, devices).unwrap();
        fs::remove_file(root.join("bin/tool")).unwrap();
        fs::write(root.join("bin/tool"), "tool\n").unwrap();
        assert_ne!(of(&root, devices).unwrap(), edited);
    }
}
