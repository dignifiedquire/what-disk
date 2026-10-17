//! Mounted volumes and the rules for which devices and paths a scan may enter.

use std::collections::HashSet;
use std::ffi::CStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSString};

#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    /// Finder name, for example "Macintosh HD".
    pub name: String,
    pub mount: PathBuf,
    pub fs_type: String,
    pub capacity: u64,
    pub available: u64,
}

impl Volume {
    /// Space in use according to the file system. On APFS this covers the whole container,
    /// including snapshots and the hidden system volumes.
    pub fn used(&self) -> u64 {
        self.capacity.saturating_sub(self.available)
    }
}

fn statfs_list() -> Vec<libc::statfs> {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo returns a pointer to a buffer owned by libc that stays valid until
    // the next call on this thread; it is copied out immediately.
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(buf, n as usize) }.to_vec()
}

fn c_str(chars: &[libc::c_char]) -> String {
    // SAFETY: statfs string fields are NUL-terminated within their fixed-size arrays.
    unsafe { CStr::from_ptr(chars.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// "/dev/disk3s1s1" -> "disk3": the APFS container a volume belongs to.
fn container_of(device: &str) -> Option<&str> {
    let rest = device.strip_prefix("/dev/disk")?;
    let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    (digits > 0).then(|| &device["/dev/".len().."/dev/disk".len() + digits])
}

pub fn display_name(path: &Path) -> String {
    let fm = NSFileManager::defaultManager();
    let name = fm.displayNameAtPath(&NSString::from_str(&path.to_string_lossy()));
    name.to_string()
}

/// Volumes a user would see in Finder's sidebar.
pub fn list() -> Vec<Volume> {
    statfs_list()
        .into_iter()
        .filter(|s| s.f_flags & libc::MNT_DONTBROWSE as u32 == 0)
        .map(|s| {
            let mount = PathBuf::from(c_str(&s.f_mntonname));
            let block = s.f_bsize as u64;
            Volume {
                name: display_name(&mount),
                fs_type: c_str(&s.f_fstypename),
                capacity: s.f_blocks * block,
                available: s.f_bavail * block,
                mount,
            }
        })
        .collect()
}

/// Decides which directories a scan of `root` may enter.
pub struct Boundary {
    devices: HashSet<u64>,
    /// Paths that are reachable twice; they are skipped here and counted at their other path.
    excluded: HashSet<PathBuf>,
}

/// Mount point of the data volume, which the system volume exposes again through firmlinks.
pub const DATA_VOLUME: &str = "/System/Volumes/Data";

impl Boundary {
    pub fn new(root: &Path) -> Self {
        let root_dev = std::fs::symlink_metadata(root).map(|m| m.dev()).ok();
        let mut devices: HashSet<u64> = root_dev.into_iter().collect();
        let mut excluded = HashSet::new();

        if root == Path::new("/") {
            // The startup disk is several APFS volumes in one container (system, data, VM,
            // preboot, update). Enter all of them, but count the data volume's firmlinked
            // folders only once, at their familiar paths (/Users, /Applications, ...).
            let mounts = statfs_list();
            let root_container = mounts
                .iter()
                .find(|s| c_str(&s.f_mntonname) == "/")
                .map(|s| c_str(&s.f_mntfromname));
            let root_container = root_container.as_deref().and_then(container_of);
            for s in &mounts {
                let on = c_str(&s.f_mntonname);
                let from = c_str(&s.f_mntfromname);
                if on.starts_with("/System/Volumes/")
                    && root_container.is_some()
                    && container_of(&from) == root_container
                    && let Ok(m) = std::fs::symlink_metadata(&on)
                {
                    devices.insert(m.dev());
                }
            }
            excluded = firmlink_targets(&std::fs::read_to_string("/usr/share/firmlinks").unwrap_or_default());
        }
        Boundary { devices, excluded }
    }

    /// Whether a directory on device `dev` may be descended into.
    pub fn allows_device(&self, dev: u64) -> bool {
        self.devices.contains(&dev)
    }

    /// Whether the entry `name` inside `parent` must be skipped entirely.
    pub fn excludes(&self, parent: &Path, name: &std::ffi::OsStr) -> bool {
        !self.excluded.is_empty()
            && parent.as_os_str().as_encoded_bytes().starts_with(DATA_VOLUME.as_bytes())
            && self.excluded.contains(&parent.join(name))
    }
}

/// Parses `/usr/share/firmlinks` ("/Users\tUsers" per line) into the data-volume paths that
/// are also reachable from the system volume.
fn firmlink_targets(table: &str) -> HashSet<PathBuf> {
    table
        .lines()
        .filter_map(|l| l.split('\t').nth(1))
        .map(|rel| Path::new(DATA_VOLUME).join(rel.trim()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_is_the_whole_disk_part_of_the_device() {
        assert_eq!(container_of("/dev/disk3s1s1"), Some("disk3"));
        assert_eq!(container_of("/dev/disk3s5"), Some("disk3"));
        assert_eq!(container_of("/dev/disk12s1"), Some("disk12"));
        assert_eq!(container_of("map auto_home"), None);
        assert_eq!(container_of("devfs"), None);
    }

    #[test]
    fn firmlinked_data_folders_are_excluded_so_they_are_not_counted_twice() {
        // /Users is the same directory as /System/Volumes/Data/Users. Counting both would
        // double the size of every home folder.
        let b = Boundary {
            devices: HashSet::new(),
            excluded: firmlink_targets("/Users\tUsers\n/System/Library/Caches\tSystem/Library/Caches\n"),
        };
        let data = Path::new(DATA_VOLUME);
        assert!(b.excludes(data, "Users".as_ref()));
        assert!(b.excludes(&data.join("System/Library"), "Caches".as_ref()));
        // Data-volume content that has no firmlink must still be counted.
        assert!(!b.excludes(data, ".Spotlight-V100".as_ref()));
        assert!(!b.excludes(&data.join("System"), "Library".as_ref()));
        // The same names elsewhere are unaffected.
        assert!(!b.excludes(Path::new("/"), "Users".as_ref()));
    }

    #[test]
    fn startup_disk_includes_its_data_volume_but_not_other_disks() {
        let b = Boundary::new(Path::new("/"));
        let dev = |p: &str| std::fs::symlink_metadata(p).unwrap().dev();
        assert!(b.allows_device(dev("/")));
        assert!(b.allows_device(dev("/System/Volumes/Data")));
        assert!(!b.allows_device(dev("/dev")));
    }
}
