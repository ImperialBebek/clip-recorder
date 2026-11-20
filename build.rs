fn main() {
    println!(
        "cargo:rustc-env=CLIP_RECORDER_ARCH={}",
        std::env::var("CARGO_CFG_TARGET_ARCH").unwrap()
    );
    slint_build::compile("ui/app.slint").expect("compile ui/app.slint");
}
