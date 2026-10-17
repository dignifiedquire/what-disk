mod app;
mod apps;
mod brew;
mod dev;
mod locations;
mod ops;
mod scan;
mod store;
mod temp;
mod volumes;

pub const BUNDLE_ID: &str = "com.dignifiedquire.whatdisk";

fn main() {
    without_large_allocation_cache();
    app::launch();
}

/// macOS keeps freed large allocations in a cache instead of returning them, and they keep
/// counting against the app. Reading a folder with hundreds of thousands of items needs
/// buffers of several megabytes, so that cache held more memory than the scan itself. The
/// cache can only be turned off before the process starts, so the app restarts itself once,
/// in place, with it off.
fn without_large_allocation_cache() {
    use std::os::unix::process::CommandExt;
    const SETTING: &str = "MallocLargeCache";
    if std::env::var_os(SETTING).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).env(SETTING, "0").exec();
    eprintln!("could not turn off the large allocation cache: {err}");
}
