mod app;
mod apps;
mod brew;
mod cache;
mod dev;
mod ops;
mod scan;
mod tree;
mod volumes;

pub const BUNDLE_ID: &str = "com.dignifiedquire.whatdisk";

fn main() {
    app::launch();
}
