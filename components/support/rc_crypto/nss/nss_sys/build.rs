/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use bindgen::Builder;
use serde_derive::Deserialize;
use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use toml;

const BINDINGS_CONFIG: &'static str = "bindings.toml";

enum LinkingKind {
    Dynamic { folded_libs: bool },
    Static,
}

// This is the format of a single section of the configuration file.
#[derive(Deserialize)]
struct Bindings {
    // The .h header files to generate from.
    headers: Vec<String>,
    // functions that are explicitly included
    functions: Option<Vec<String>>,
    // types that are explicitly included
    types: Option<Vec<String>>,
    // (un-used) functions that are explicitly included
    // functions: Option<Vec<String>>,
    // variables (and `#define`s) that are explicitly included
    variables: Option<Vec<String>>,
    // types that should be explicitly marked as opaque
    opaque: Option<Vec<String>>,
    // enumerations that are turned into a module (without this, the enum is
    // mapped using the default, which means that the individual values are
    // formed with an underscore as <enum_type>_<enum_value_name>).
    enums: Option<Vec<String>>,

    // Any item that is specifically excluded; if none of the types, functions,
    // or variables fields are specified, everything defined will be mapped,
    // so this can be used to limit that.
    exclude: Option<Vec<String>>,
}

fn env(name: &str) -> Option<OsString> {
    println!("cargo:rerun-if-env-changed={}", name);
    env::var_os(name)
}

fn env_str(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={}", name);
    env::var(name).ok()
}

fn main() {
    // 1. NSS linking.
    let (lib_dir, include_dir) = get_nss();
    println!(
        "cargo:rustc-link-search=native={}",
        lib_dir.to_string_lossy()
    );
    println!("cargo:include={}", include_dir.to_string_lossy());
    let kind = determine_kind();
    link_nss_libs(&kind);
    // 2. Bindings.
    let config_file = PathBuf::from(BINDINGS_CONFIG);
    println!("cargo:rerun-if-changed={}", config_file.to_str().unwrap());
    let config = fs::read_to_string(config_file).expect("unable to read binding configuration");
    let bindings: Bindings = toml::from_str(&config).unwrap();
    build_bindings(&bindings, &include_dir.join("nss"));
}

fn determine_kind() -> LinkingKind {
    return match env_str("NSS_STATIC").as_ref().map(String::as_ref) {
        Some("1") => LinkingKind::Static,
        _ => {
            let folded_libs = match env_str("NSS_USE_FOLDED_LIBS").as_ref().map(String::as_ref) {
                Some("1") => true,
                _ => false,
            };
            return LinkingKind::Dynamic { folded_libs };
        }
    };
}

fn link_nss_libs(kind: &LinkingKind) {
    let libs = get_nss_libs(kind);
    // Emit -L flags
    let kind_str = match kind {
        LinkingKind::Dynamic { .. } => "dylib",
        LinkingKind::Static => "static",
    };
    for lib in libs {
        println!("cargo:rustc-link-lib={}={}", kind_str, lib);
    }
}

fn get_nss_libs(kind: &LinkingKind) -> Vec<&'static str> {
    match kind {
        LinkingKind::Static => {
            let mut static_libs = vec![
                "certdb",
                "certhi",
                "cryptohi",
                "freebl_static",
                "hw-acc-crypto",
                "nspr4",
                "nss_static",
                "nssb",
                "nssdev",
                "nsspki",
                "nssutil",
                "pk11wrap_static",
                "plc4",
                "plds4",
                "softokn_static",
            ];
            // Hardware specific libs.
            let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
            let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
            // https://searchfox.org/mozilla-central/rev/1eb05019f47069172ba81a6c108a584a409a24ea/security/nss/lib/freebl/freebl.gyp#159-168
            if target_arch == "x86_64" || target_arch == "x86" {
                static_libs.push("gcm-aes-x86_c_lib");
            } else if target_arch == "aarch64" {
                static_libs.push("gcm-aes-aarch64_c_lib");
            }
            // https://searchfox.org/mozilla-central/rev/1eb05019f47069172ba81a6c108a584a409a24ea/security/nss/lib/freebl/freebl.gyp#224-233
            if ((target_os == "android" || target_os == "linux") && target_arch == "x86_64")
                || target_os == "windows"
            {
                static_libs.push("intel-gcm-wrap_c_lib");
                // https://searchfox.org/mozilla-central/rev/1eb05019f47069172ba81a6c108a584a409a24ea/security/nss/lib/freebl/freebl.gyp#43-47
                if (target_os == "android" || target_os == "linux") && target_arch == "x86_64" {
                    static_libs.push("intel-gcm-s_lib");
                }
            }
            return static_libs;
        }
        LinkingKind::Dynamic { folded_libs } => {
            let mut dylibs = vec!["freebl3", "nss3", "nssckbi", "softokn3"];
            if !folded_libs {
                dylibs.append(&mut vec!["nspr4", "nssutil3", "plc4", "plds4"]);
            }
            return dylibs;
        }
    }
}

fn get_nss() -> (PathBuf, PathBuf) {
    let nss_dir = env("NSS_DIR").expect("To build nss_sys, NSS_DIR must be set!");
    let nss_dir = Path::new(&nss_dir);
    let lib_dir = nss_dir.join("lib");
    let include_dir = nss_dir.join("include");
    (lib_dir, include_dir)
}

fn build_bindings(bindings: &Bindings, include_dir: &PathBuf) {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("nss_bindings.rs");
    let mut builder = Builder::default().generate_comments(false);

    for h in bindings.headers.iter().cloned() {
        builder = builder.header(include_dir.join(h).to_str().unwrap());
    }

    // Fix our cross-compilation include directories.
    builder = fix_include_dirs(builder);

    // Apply the configuration.
    let empty: Vec<String> = vec![];
    for v in bindings.types.as_ref().unwrap_or_else(|| &empty).iter() {
        builder = builder.whitelist_type(v);
    }
    for v in bindings.functions.as_ref().unwrap_or_else(|| &empty).iter() {
        builder = builder.whitelist_function(v);
    }
    for v in bindings.variables.as_ref().unwrap_or_else(|| &empty).iter() {
        builder = builder.whitelist_var(v);
    }
    for v in bindings.exclude.as_ref().unwrap_or_else(|| &empty).iter() {
        builder = builder.blacklist_item(v);
    }
    for v in bindings.opaque.as_ref().unwrap_or_else(|| &empty).iter() {
        builder = builder.opaque_type(v);
    }
    for v in bindings.enums.as_ref().unwrap_or_else(|| &empty).iter() {
        builder = builder.constified_enum_module(v);
    }

    let bindings = builder.generate().expect("unable to generate bindings");
    bindings
        .write_to_file(out)
        .expect("couldn't write bindings");
}

fn fix_include_dirs(mut builder: Builder) -> Builder {
    let target_os = env::var("CARGO_CFG_TARGET_OS");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH");
    match target_os.as_ref().map(|x| &**x) {
        Ok("macos") => {
            // Cheap and dirty way to detect that we are cross-compiling.
            if env::var_os("CI").is_some() {
                builder = builder
                    .detect_include_paths(false)
                    .clang_arg("-isysroot/tmp/MacOSX10.11.sdk");
            }
        }
        Ok("windows") => {
            if env::var_os("CI").is_some() {
                builder = builder.clang_arg("-D_M_X64");
            }
        }
        Ok("ios") => {
            let sdk_root;
            match target_arch.as_ref().map(|x| &**x).unwrap() {
                "aarch64" => {
                    sdk_root = get_ios_sdk_root("iphoneos");
                    builder = builder.clang_arg("--target=arm64-apple-ios") // See https://github.com/rust-lang/rust-bindgen/issues/1211
                }
                "x86_64" => {
                    sdk_root = get_ios_sdk_root("iphonesimulator");
                }
                _ => panic!("Unknown iOS architecture."),
            }
            builder = builder
                .detect_include_paths(false)
                .clang_arg(format!("-isysroot{}", &sdk_root));
        }
        Ok("android") => {
            let android_ndk_root = PathBuf::from(env::var("ANDROID_NDK_ROOT").unwrap());
            builder = builder.detect_include_paths(false).clang_arg(format!(
                "--sysroot={}",
                &android_ndk_root
                    .join(format!(
                        "toolchains/llvm/prebuilt/{}/sysroot",
                        android_host_tag()
                    ))
                    .to_str()
                    .unwrap()
            ));
            if cfg!(target_os = "linux") {
                // stddef.h isn't defined otherwise.
                builder = builder.clang_arg(format!(
                    "-I{}",
                    &android_ndk_root
                        .join(format!(
                            "toolchains/llvm/prebuilt/{}/lib64/clang/8.0.7/include/",
                            android_host_tag()
                        ))
                        .to_str()
                        .unwrap()
                ));
            }
        }
        _ => {}
    }
    return builder;
}

fn android_host_tag() -> &'static str {
    // cfg! target_os actually refers to the host environment in this case (build script).
    #[cfg(target_os = "macos")]
    return "darwin-x86_64";
    #[cfg(target_os = "linux")]
    return "linux-x86_64";
    #[cfg(target_os = "windows")]
    return "windows-x86_64";
}

fn get_ios_sdk_root(sdk_name: &str) -> String {
    let output = Command::new("xcrun")
        .arg("--show-sdk-path")
        .arg("-sdk")
        .arg(sdk_name)
        .output()
        .unwrap();
    if output.status.success() {
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    } else {
        panic!("Could not get iOS SDK root!")
    }
}
