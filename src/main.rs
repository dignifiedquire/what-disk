mod app;
mod apps;
mod brew;
mod cache;
mod dev;
mod locations;
mod ops;
mod scan;
// Not used by the app yet: the scanner and views move to it in the next steps.
#[allow(dead_code)]
mod store;
mod temp;
mod tree;
mod volumes;

pub const BUNDLE_ID: &str = "com.dignifiedquire.whatdisk";

fn main() {
    app::launch();
}
