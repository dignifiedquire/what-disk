mod app;
mod cache;
mod ops;
mod scan;
mod tree;
mod volumes;

pub const BUNDLE_ID: &str = "com.dignifiedquire.whatdisk";

fn main() {
    app::launch();
}
