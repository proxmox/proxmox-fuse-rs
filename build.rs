use std::env;
use std::process::Command;

fn main() {
    let ccflags = Command::new("pkgconf")
        .arg("--cflags")
        .arg("fuse3")
        .output()
        .expect("failed to get fuse3 cflags");
    let ccflags = std::str::from_utf8(&ccflags.stdout).expect("expected cflags to be utf8");

    let mut cc = cc::Build::new();

    cc.pic(true)
        .shared_flag(false)
        .opt_level(3)
        .flag("-DFUSE_USE_VERSION=35");

    for flag in ccflags.split_ascii_whitespace() {
        cc.flag(flag);
    }

    cc.file("src/glue.c").compile("libglue.a");

    // the debian package should include src/glue.c
    println!(
        "dh-cargo:deb-built-using=glue=1={}",
        env::var("CARGO_MANIFEST_DIR").unwrap()
    );
}
