use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let ico_path = format!("{}\\assets\\icons\\dy-icon.ico", manifest_dir);
    let rc_file = format!("{}\\resource.rc", out_dir);
    let res_file = format!("{}\\resource.res", out_dir);

    let rc_content = format!("1 ICON \"{}\"", ico_path.replace('\\', "\\\\"));
    std::fs::write(&rc_file, rc_content).expect("failed to write resource.rc");

    let rc_exe = find_rc_exe().expect("failed to find rc.exe in Windows SDK");
    let status = Command::new(&rc_exe)
        .arg(format!("/fo{}", res_file))
        .arg(&rc_file)
        .status()
        .expect("failed to run rc.exe");
    assert!(status.success(), "rc.exe failed to compile resource");

    println!("cargo:rustc-link-arg={}", res_file);
    println!("cargo:rerun-if-changed=assets/icons/dy-icon.ico");
}

fn find_rc_exe() -> Option<String> {
    let base = r"C:\Program Files (x86)\Windows Kits\10\bin";
    let mut latest: Option<(String, String)> = None;
    for entry in std::fs::read_dir(base).ok()?.flatten() {
        let ver = entry.file_name().to_string_lossy().into_owned();
        let p = entry.path().join("x64").join("rc.exe");
        if p.exists() && ver.starts_with("10.") {
            match &latest {
                Some((v, _)) if v >= &ver => {}
                _ => latest = Some((ver, p.to_string_lossy().into_owned())),
            }
        }
    }
    latest.map(|(_, p)| p)
}