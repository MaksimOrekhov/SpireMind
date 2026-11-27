use std::{fs, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=pins/upstream.rev");
    for entry in ["HEAD", "index"] {
        println!("cargo:rerun-if-changed=.local/upstream/.git/{entry}");
    }
    let expected = fs::read_to_string("pins/upstream.rev").expect("read upstream pin");
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-C", ".local/upstream"])
            .args(args)
            .output()
            .expect("run git for the Spire Mentor checkout")
    };
    let rev = git(&["rev-parse", "--verify", "HEAD"]);
    let status = git(&["status", "--porcelain", "--untracked-files=no"]);
    assert!(
        rev.status.success()
            && String::from_utf8_lossy(&rev.stdout).trim() == expected.trim()
            && status.status.success()
            && status.stdout.is_empty(),
        "expected a clean Spire Mentor checkout at {}; run python scripts/setup.py --source-only",
        expected.trim()
    );
    println!("cargo:rustc-env=SPIRE_MENTOR_REV={}", expected.trim());
}
