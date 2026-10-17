mod app;
mod apps;
mod brew;
mod cache;
mod dev;
mod locations;
mod ops;
mod scan;
mod temp;
mod tree;
mod volumes;

pub const BUNDLE_ID: &str = "com.dignifiedquire.whatdisk";

fn main() {
    app::launch();
}
