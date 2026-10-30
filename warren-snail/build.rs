//! With the feature `collations`, links the machine's ICU as PostgreSQL links it: found by
//! pkg-config as `icu-uc` and `icu-i18n`, its entry points named with the version suffix its own
//! headers give them (`uvernum.h`, unless `uconfig.h` turns the renaming off). Without the feature
//! it does nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_COLLATIONS").is_none() {
        return;
    }
    let include = pkg_config(&["--variable=includedir", "icu-uc"])
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or_else(|| PathBuf::from("/usr/include"));
    let suffix = if renaming_off(&include) {
        String::new()
    } else {
        define(&include.join("unicode/uvernum.h"), "U_ICU_VERSION_SUFFIX").unwrap_or_else(|| {
            panic!(
                "the feature collations links ICU, and {} names no U_ICU_VERSION_SUFFIX: install \
                 ICU's development files",
                include.join("unicode/uvernum.h").display()
            )
        })
    };
    println!("cargo:rustc-env=WARREN_SNAIL_ICU_SUFFIX={suffix}");
    println!(
        "cargo:rerun-if-changed={}",
        include.join("unicode/uvernum.h").display()
    );
    match pkg_config(&["--libs", "icu-uc", "icu-i18n"]) {
        Some(libs) => {
            for flag in libs.split_whitespace() {
                if let Some(dir) = flag.strip_prefix("-L") {
                    println!("cargo:rustc-link-search=native={dir}");
                } else if let Some(lib) = flag.strip_prefix("-l") {
                    println!("cargo:rustc-link-lib=dylib={lib}");
                }
            }
        }
        None => {
            for lib in ["icui18n", "icuuc", "icudata"] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
    }
}

fn pkg_config(args: &[&str]) -> Option<String> {
    let out = Command::new("pkg-config").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The value a header `#define`s a name to, as written.
fn define(header: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(header).ok()?;
    text.lines().find_map(|l| {
        let mut w = l.trim().strip_prefix('#')?.split_whitespace();
        (w.next()? == "define" && w.next()? == name).then(|| w.next().unwrap_or("").to_string())
    })
}

fn renaming_off(include: &Path) -> bool {
    define(&include.join("unicode/uconfig.h"), "U_DISABLE_RENAMING").is_some_and(|v| v == "1")
}
