use std::path::PathBuf;
use std::process::Command;

fn pilisp_binary() -> PathBuf {
    PathBuf::from(std::env!("CARGO_BIN_EXE_pilisp"))
}

fn run_file(file: &str) -> (bool, String) {
    let output = Command::new(pilisp_binary())
        .arg(file)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run pilisp");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.success(), format!("{}{}", stdout, stderr))
}

#[test]
fn test_hello_pi() {
    let (ok, msg) = run_file("hello.pi");
    assert!(ok, "hello.pi failed:\n{}", msg);
    assert!(msg.contains("=> ()"));
}

#[test]
fn test_test_pi() {
    let (ok, msg) = run_file("test/test.pi");
    assert!(ok, "test/test.pi failed:\n{}", msg);
}
