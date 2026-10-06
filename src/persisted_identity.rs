//! `device:inode` identities read back by a later process, possibly after a
//! reboot.
//!
//! Shdeps journals a filesystem object's `st_dev`/`st_ino` and later proves
//! that the object at a path is still the one it recorded, not one recreated,
//! copied, or restored since, before trusting or deleting it. The journals
//! are the method-transition records (`cleanup::Evidence` and the release
//! archive root), the public-command transition record, the prune hook
//! records, and the repository-root transition record.
//!
//! The inode number belongs to the object, but the device number belongs to
//! the mount: macOS assigns APFS volumes their `st_dev` at mount time, Linux
//! gives btrfs (and every other anonymous-device filesystem) one at mount,
//! and device-mapper minors follow activation order. A reboot can renumber
//! the device under an unchanged object, so comparing the recorded device
//! exactly refuses every pending journal after a plain reboot, and a refused
//! journal makes each later `shdeps update` and `shdeps prune` fail or leave
//! the old payload behind.
//!
//! A recorded identity still names a live object when the inode matches and
//! either the device matches too (the exact rule, unchanged) or the object's
//! birth time is known and no later than the time its journal was last
//! written. The identity was read from the object before the journal holding
//! it was written, so the recorded object was born before the journal, while
//! an object recreated after that was born later. A file-level copy or
//! restore gets a new inode, which is what refuses it: on macOS a
//! time-preserving copy (`cp -p`, `ditto`, Time Machine) can carry the birth
//! time back with it, but APFS never reuses an inode number on a volume.
//! Birth time survives remounts because filesystems store it with the inode
//! (APFS, btrfs `otime`, ext4 `crtime`, XFS v5).
//!
//! Where no birth time is available, the device must still match exactly, so
//! the check is never weaker than the exact rule. That covers Android (std
//! reports no birth time there, and `statx` is not tried because older app
//! seccomp filters kill the process on it), filesystems that store none, and
//! kernels older than `statx`.
//!
//! Trade-offs. An object that reuses the recorded inode number on the same
//! device passes, exactly as before. A changed device widens that to an
//! object that also has the recorded inode number and already existed when
//! the journal was written:
//! - a block-level restore or snapshot rollback that keeps inode numbers and
//!   birth times (a btrfs subvolume swap, an APFS snapshot revert) now reads
//!   as the same object; the other facts each identity records (timestamps,
//!   mode, size, symlink target, the managed-root tree fingerprint) still
//!   apply;
//! - filesystem roots have fixed inode numbers (2 on ext4 and APFS, 256 for
//!   a btrfs subvolume), so an older filesystem mounted where a recorded
//!   root was also matches; excluding mount roots would refuse the common
//!   btrfs home that is a subvolume root after every reboot;
//! - an identity is captured before its journal's first write, sometimes
//!   across a checkout-lock wait, and journals rewritten for a later phase
//!   move the bound later still; a reader keeps the bound it observed before
//!   this process rewrote the journal;
//! - the comparison trusts the clocks that stamped the birth time and the
//!   journal: after a reboot with the clock behind (no hardware clock, before
//!   time sync), or across filesystems whose clocks disagree (a lagging NFS
//!   server), an object recreated after the journal can carry an earlier
//!   birth time and pass if it also reuses the recorded inode number (btrfs,
//!   for one, recomputes its next inode number at mount).
//!
//! None of these is weaker than the exact rule on a device that was never
//! renumbered, where inode reuse alone already passed.
//!
//! Recording the birth time instead would have changed the journal formats,
//! and older Shdeps releases parse them strictly (`deny_unknown_fields`), so
//! a downgrade would refuse every record a newer Shdeps wrote. Bounding by
//! the journal's own modification time needs no format change and also
//! covers records written before this rule existed.

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::time::SystemTime;

/// When the journal a recorded identity was read from was last written: the
/// bound a renumbered device is held to. Identities this process captured
/// itself carry no bound and keep the exact rule.
///
/// The bound describes where an identity was read from, not the object it
/// names, so it is never serialized and never part of equality: two
/// snapshots of the same objects compare equal whichever journal they came
/// from, and comparing a recorded snapshot with a fresh one stays exact.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Journaled(Option<SystemTime>);

impl Journaled {
    /// The bound for identities read from `journal`, or none (the exact
    /// rule) when its modification time cannot be read.
    pub(crate) fn at(journal: &Path) -> Self {
        Self(
            std::fs::symlink_metadata(journal)
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    }
}

impl PartialEq for Journaled {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Journaled {}

/// Whether the object at `path`, already `lstat`ed into `metadata`, is the
/// one recorded as `device`/`inode` in a journal bounded by `journaled` (see
/// the module docs).
pub(crate) fn matches(
    device: u64,
    inode: u64,
    path: &Path,
    metadata: &Metadata,
    journaled: Journaled,
) -> bool {
    decide(
        (device, inode),
        (metadata.dev(), metadata.ino()),
        || known(birth_time(path, metadata)),
        journaled.0,
    )
}

/// The rule itself, on plain values. `birth` is consulted only for a
/// renumbered device, so the common exact match costs no extra syscall.
fn decide(
    (device, inode): (u64, u64),
    (live_device, live_inode): (u64, u64),
    birth: impl FnOnce() -> Option<SystemTime>,
    journaled: Option<SystemTime>,
) -> bool {
    if live_inode != inode {
        return false;
    }
    if live_device == device {
        return true;
    }
    journaled.is_some_and(|journaled| birth().is_some_and(|birth| birth <= journaled))
}

/// A reported birth time, or `None` for the epoch itself: some filesystems
/// report zero for "unknown", which would predate every journal.
fn known(birth: Option<SystemTime>) -> Option<SystemTime> {
    birth.filter(|birth| *birth > SystemTime::UNIX_EPOCH)
}

/// Whether this platform and filesystem report a birth time for the object
/// at `path`, so tests can tell the renumbering rule from the exact one.
#[cfg(test)]
pub(crate) fn reports_birth_time(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| known(birth_time(path, &metadata)).is_some())
}

/// Birth time of the object `metadata` describes (`lstat` of `path`). Linux
/// reads it through `statx` directly: std does that only on glibc, and
/// release binaries are static musl builds.
#[cfg(target_os = "linux")]
fn birth_time(path: &Path, metadata: &Metadata) -> Option<SystemTime> {
    use std::os::unix::ffi::OsStrExt as _;

    // The kernel's `struct statx` (include/uapi/linux/stat.h), the same on
    // every architecture. libc declares it only for glibc and Android (musl
    // needs an opt-in cfg), so the fields read here are laid out locally;
    // the assertions below pin their offsets to the UAPI layout.
    #[repr(C)]
    struct Statx {
        mask: u32,
        _head: [u8; 28],
        ino: u64,
        _middle: [u8; 40],
        btime_sec: i64,
        btime_nsec: u32,
        _between: [u8; 44],
        dev_major: u32,
        dev_minor: u32,
        _tail: [u8; 112],
    }
    const _: () = assert!(std::mem::size_of::<Statx>() == 256);
    const _: () = assert!(std::mem::offset_of!(Statx, ino) == 32);
    const _: () = assert!(std::mem::offset_of!(Statx, btime_sec) == 80);
    const _: () = assert!(std::mem::offset_of!(Statx, dev_major) == 136);
    const _: () = assert!(std::mem::offset_of!(Statx, dev_minor) == 140);
    const STATX_INO: libc::c_uint = 0x100;
    const STATX_BTIME: libc::c_uint = 0x800;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // Every identity here comes from `lstat`, which never follows the last
    // symlink or triggers an automount; `statx` does both unless told not to.
    let flags = libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT;
    let mut buffer = std::mem::MaybeUninit::<Statx>::zeroed();
    // SAFETY: `path` is NUL-terminated and outlives the call, and `buffer`
    // is a writable 256-byte `struct statx`. A kernel without `statx`
    // (ENOSYS) or a seccomp filter that refuses it (EPERM) fails the call,
    // which reads as "no birth time".
    let status = unsafe {
        // Every argument widened to `long`, the way std passes them: the
        // variadic `syscall` reads each one as a `long`.
        libc::syscall(
            libc::SYS_statx,
            libc::c_long::from(libc::AT_FDCWD),
            path.as_ptr(),
            libc::c_long::from(flags),
            // A two-bit mask: lossless as a `long` on every target.
            (STATX_INO | STATX_BTIME) as libc::c_long,
            buffer.as_mut_ptr(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: the zeroed buffer is a valid `Statx` (plain integers), now
    // filled by a successful call.
    let statx = unsafe { buffer.assume_init() };
    // The path may have been replaced between the two stats; a birth time
    // from another object (even one with the same inode number on another
    // device) proves nothing. The device fields are always filled.
    if statx.mask & STATX_BTIME == 0
        || statx.mask & STATX_INO == 0
        || statx.ino != metadata.ino()
        || libc::makedev(statx.dev_major, statx.dev_minor) != metadata.dev()
    {
        return None;
    }
    let since_epoch =
        std::time::Duration::new(u64::try_from(statx.btime_sec).ok()?, statx.btime_nsec);
    SystemTime::UNIX_EPOCH.checked_add(since_epoch)
}

/// Birth time of the object `metadata` describes, where std reports one
/// (`st_birthtime` on macOS; none on Android).
#[cfg(not(target_os = "linux"))]
fn birth_time(_path: &Path, metadata: &Metadata) -> Option<SystemTime> {
    metadata.created().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(seconds: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
    }

    fn never() -> Option<SystemTime> {
        panic!("an exact or inode-mismatched identity must not read a birth time")
    }

    #[test]
    fn an_unchanged_identity_matches_without_a_birth_time() {
        assert!(decide(
            (16777229, 377670759),
            (16777229, 377670759),
            never,
            None
        ));
        assert!(decide((1, 7), (1, 7), never, at(2_000)));
    }

    #[test]
    fn a_renumbered_device_matches_an_object_born_before_its_journal() {
        // A Mac after a reboot: same inode, APFS device renumbered.
        let recorded = (16777229, 377670759);
        let live = (16777231, 377670759);
        assert!(decide(recorded, live, || at(1_000), at(2_000)));
        assert!(decide(recorded, live, || at(1_000), at(1_000)));
    }

    #[test]
    fn a_renumbered_device_refuses_an_object_born_after_its_journal() {
        // Recreated (reusing the inode number) after the journal was written.
        assert!(!decide((1, 7), (2, 7), || at(3_000), at(2_000)));
    }

    #[test]
    fn a_renumbered_device_needs_both_a_birth_time_and_a_journal_time() {
        assert!(!decide((1, 7), (2, 7), || None, at(2_000)));
        assert!(!decide((1, 7), (2, 7), || at(1_000), None));
    }

    #[test]
    fn another_inode_never_matches() {
        // A copy or restore: new inode, whatever the device or birth.
        assert!(!decide((1, 7), (1, 8), never, at(2_000)));
        assert!(!decide((1, 7), (2, 8), never, at(2_000)));
    }

    #[test]
    fn a_zero_birth_time_counts_as_unknown() {
        assert_eq!(known(Some(SystemTime::UNIX_EPOCH)), None);
        assert_eq!(known(at(1)), at(1));
        assert_eq!(known(None), None);
    }

    #[test]
    fn the_journal_bound_is_not_part_of_equality() {
        let dir = crate::test_support::temp_dir("persisted-identity-equality");
        assert_eq!(Journaled::at(&dir), Journaled::default());
        assert!(Journaled::at(&dir).0.is_some());
        assert!(Journaled::at(&dir.join("missing")).0.is_none());
    }

    #[test]
    fn birth_time_belongs_to_the_object_it_names() {
        // Where the platform reports a birth time, a fresh symlink's own
        // (`lstat`) birth time is recent. On Linux the `statx` result is
        // kept only for the inode the metadata describes, so pairing the
        // link's path with its target's metadata yields none.
        let dir = crate::test_support::temp_dir("persisted-identity-birth");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let metadata = std::fs::symlink_metadata(&link).unwrap();
        let birth = known(birth_time(&link, &metadata));
        if let Some(birth) = birth {
            let age = SystemTime::now().duration_since(birth).unwrap_or_default();
            assert!(age < Duration::from_secs(600), "{age:?}");
        }
        // Metadata for another object proves nothing about this path.
        let other = std::fs::symlink_metadata(&target).unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(birth_time(&link, &other), None);
        }
    }

    #[test]
    fn birth_time_agrees_with_std_where_std_reports_one() {
        // The renumbering tests fall back to the exact rule where no birth
        // time is reported, so a broken `statx` (or `created()`) path would
        // quietly pass them. std reads the same birth time on glibc Linux
        // (through `statx`) and on macOS, where APFS always stores one.
        let dir = crate::test_support::temp_dir("persisted-identity-std");
        let metadata = std::fs::symlink_metadata(&dir).unwrap();
        let ours = known(birth_time(&dir, &metadata));
        if let Ok(created) = metadata.created() {
            assert_eq!(ours, known(Some(created)));
        }
        if cfg!(target_os = "macos") {
            assert!(ours.is_some());
        }
    }

    #[test]
    fn a_renumbered_live_object_matches_through_the_filesystem() {
        // End to end through `lstat` and the real birth time: the recorded
        // device is wrong, the inode is right, and the journal was written
        // after the object was born.
        let dir = crate::test_support::temp_dir("persisted-identity-live");
        let object = dir.join("object");
        std::fs::create_dir(&object).unwrap();
        let journal = dir.join("journal");
        std::fs::write(&journal, "{}\n").unwrap();
        let metadata = std::fs::symlink_metadata(&object).unwrap();
        let renumbered = metadata.dev() + 1;
        let bound = Journaled::at(&journal);

        assert!(matches(
            metadata.dev(),
            metadata.ino(),
            &object,
            &metadata,
            Journaled::default()
        ));
        assert!(!matches(
            renumbered,
            metadata.ino(),
            &object,
            &metadata,
            Journaled::default()
        ));
        assert_eq!(
            matches(renumbered, metadata.ino(), &object, &metadata, bound),
            reports_birth_time(&object)
        );
        crate::test_support::set_journal_time(
            &journal,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        );
        assert!(!matches(
            renumbered,
            metadata.ino(),
            &object,
            &metadata,
            Journaled::at(&journal)
        ));
    }
}
