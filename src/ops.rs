//! Actions on files and integration with Finder.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use objc2::AllocAnyThread;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext,
    NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

/// Moves `path` to the Trash of its volume. Uses NSFileManager directly rather than
/// scripting Finder, which would ask for Automation permission.
pub fn move_to_trash(path: &Path) -> io::Result<()> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos};
    let mut ctx = trash::TrashContext::default();
    ctx.set_delete_method(DeleteMethod::NsFileManager);
    ctx.delete(path).map_err(|e| io::Error::other(e.to_string()))
}

/// Deletes `path` permanently. Symbolic links are removed, never followed.
pub fn delete_now(path: &Path) -> io::Result<()> {
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return std::fs::remove_file(path);
    }
    match std::fs::remove_dir_all(path) {
        // Go's module cache, among others, is read-only on purpose. Its owner may still delete
        // it once the folders are writable again.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            make_folders_writable(path)?;
            std::fs::remove_dir_all(path)
        }
        result => result,
    }
}

/// Gives the owner write access to `path` and every folder below it, without following
/// symbolic links.
fn make_folders_writable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut permissions = std::fs::symlink_metadata(&dir)?.permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        std::fs::set_permissions(&dir, permissions)?;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

fn spawn_quiet(cmd: &mut Command) {
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

pub fn reveal_in_finder(path: &Path) {
    spawn_quiet(Command::new("/usr/bin/open").arg("-R").arg(path));
}

thread_local! {
    /// The system Quick Look panel; it exists once per app and only on the main thread.
    static QUICK_LOOK: Option<quicklook::QuickLookPanel> = quicklook::QuickLookPanel::shared();
}

/// Shows `path` in the system Quick Look panel, or closes the panel if it already shows that
/// item, as Space does in Finder. Does nothing off the main thread.
pub fn quick_look(path: &Path) {
    QUICK_LOOK.with(|panel| {
        let (Some(panel), Some(item)) = (panel, quicklook::PreviewItem::from_file_url(path, None))
        else {
            return;
        };
        let showing_it = panel.is_visible()
            && panel.current_preview_item().and_then(|i| i.absolute_url_string())
                == item.absolute_url_string();
        if showing_it {
            panel.hide();
            return;
        }
        panel.set_items(vec![item]);
        panel.reload_if_dirty();
        panel.show();
    });
}

pub fn open_full_disk_access_settings() {
    spawn_quiet(
        Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"),
    );
}

/// The TCC database is readable only by processes with Full Disk Access.
pub fn has_full_disk_access() -> bool {
    std::fs::File::open("/Library/Application Support/com.apple.TCC/TCC.db").is_ok()
}

/// Finder's icon for `path` as a square PNG of `pixels` × `pixels`.
pub fn icon_png(path: &Path, pixels: isize) -> Option<Vec<u8>> {
    let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&path.to_string_lossy()));
    // SAFETY: a null planes pointer makes the rep allocate its own buffer; the arguments
    // describe a valid 8-bit RGBA layout.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            pixels,
            pixels,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }?;
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    let side = pixels as f64;
    image.drawInRect(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side)));
    NSGraphicsContext::restoreGraphicsState_class();
    // SAFETY: an empty properties dictionary is valid for PNG encoding.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_now_removes_folders_and_does_not_follow_symlinks() {
        // Deleting a symlink to a folder must never delete the folder it points to.
        let tmp = tempfile::tempdir().unwrap();
        let keep = tmp.path().join("keep");
        std::fs::create_dir(&keep).unwrap();
        std::fs::write(keep.join("precious"), b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&keep, &link).unwrap();
        let doomed = tmp.path().join("doomed/nested");
        std::fs::create_dir_all(&doomed).unwrap();
        std::fs::write(doomed.join("f"), b"x").unwrap();

        delete_now(&link).unwrap();
        delete_now(&tmp.path().join("doomed")).unwrap();
        assert!(!link.exists() && !tmp.path().join("doomed").exists());
        assert!(keep.join("precious").exists());
    }

    #[test]
    fn delete_now_removes_read_only_folders_of_the_user() {
        // Go makes its module cache read-only; "Delete Immediately" must still work on it.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("mod");
        let module = cache.join("example.com/lib@v1.0.0");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("lib.go"), b"package lib").unwrap();
        for dir in [&module, &module.parent().unwrap().to_path_buf(), &cache] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        delete_now(&cache).unwrap();
        assert!(!cache.exists());
    }

    #[test]
    fn icons_render_as_png() {
        let png = icon_png(Path::new("/Applications"), 32).expect("icon");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
