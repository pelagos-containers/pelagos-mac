use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Designated requirement for the calling binary (pelagos-mac).
    // For development: create a local "pelagos-mac Dev" certificate in Keychain Access
    //   (Keychain Access → Certificate Assistant → Create a Certificate →
    //    Name: "pelagos-mac Dev", Certificate Type: Code Signing).
    // For production: set PELAGOS_CALLER_DR to the Developer ID designated requirement.
    let caller_dr = env::var("PELAGOS_CALLER_DR")
        .unwrap_or_else(|_| r#"certificate leaf[subject.CN] = "pelagos-mac Dev""#.to_string());

    // Substitute @CALLER_DR@ in the template and write to OUT_DIR.
    let template = fs::read_to_string(manifest.join("assets/com.pelagos.pfctl.embedded.plist.in"))
        .expect("pelagos-pfctl/assets/com.pelagos.pfctl.embedded.plist.in not found");
    let plist = template.replace("@CALLER_DR@", &caller_dr);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let plist_path = out_dir.join("com.pelagos.pfctl.embedded.plist");
    fs::write(&plist_path, &plist).unwrap();

    // Embed as __TEXT,__launchd_plist Mach-O section.
    // SMJobBless reads this section from the helper binary to obtain the
    // LaunchDaemon plist and validate SMAuthorizedClients.
    println!(
        "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__launchd_plist,{}",
        plist_path.display()
    );

    println!("cargo:rerun-if-changed=assets/com.pelagos.pfctl.embedded.plist.in");
    println!("cargo:rerun-if-env-changed=PELAGOS_CALLER_DR");
}
