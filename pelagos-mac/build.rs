use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Git hash for version string.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={}", hash);
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Designated requirement for the helper binary (pelagos-pfctl).
    // Must match the SMAuthorizedClients entry in pelagos-pfctl's embedded plist.
    // For development: matches the "pelagos-mac Dev" local certificate.
    // For production: set PELAGOS_HELPER_DR to the Developer ID designated requirement.
    let helper_dr = env::var("PELAGOS_HELPER_DR")
        .unwrap_or_else(|_| r#"certificate leaf[subject.CN] = "pelagos-mac Dev""#.to_string());

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let template = fs::read_to_string(manifest.join("assets/Info.plist.in"))
        .expect("pelagos-mac/assets/Info.plist.in not found");
    let plist = template.replace("@HELPER_DR@", &helper_dr);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let plist_path = out_dir.join("Info.plist");
    fs::write(&plist_path, &plist).unwrap();

    // Embed as __TEXT,__info_plist Mach-O section.
    // SMJobBless reads this from the calling binary to validate SMPrivilegedExecutables.
    println!(
        "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist_path.display()
    );

    println!("cargo:rerun-if-changed=assets/Info.plist.in");
    println!("cargo:rerun-if-env-changed=PELAGOS_HELPER_DR");
}
