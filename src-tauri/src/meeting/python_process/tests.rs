//! Exercise worker pipes without requiring the multi-GB speech model.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::python_command;

const PROBE_MODE: &str = "SNACK_PYTHON_COMMAND_TEST_MODE";
const PROBE_ARGUMENT: &str = "录音文件夹/会议 记录 & \"讨论\".wav";
const PROBE_TEST: &str = "meeting::python_process::tests::background_worker_probe";

#[test]
fn background_worker_preserves_progress_result_and_stderr() {
    let output = run_probe("success");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let messages = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    assert_eq!(
        messages,
        vec![
            serde_json::json!({ "type": "progress", "percent": 2, "remainingSeconds": 30 }),
            serde_json::json!({ "type": "result", "text": "会议内容", "segments": [] }),
        ]
    );
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("worker diagnostic"));
}

#[test]
fn background_worker_preserves_unsuccessful_exit_and_diagnostic() {
    let output = run_probe("failure");
    assert_eq!(output.status.code(), Some(23));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("worker diagnostic"));
}

#[test]
fn background_worker_keeps_nested_workers_without_a_console() {
    let output = run_probe("nested");
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("会议内容"));
}

/// Run only in a spawned test process so console and stdin checks are isolated.
#[test]
fn background_worker_probe() {
    let Ok(mode) = std::env::var(PROBE_MODE) else {
        return;
    };
    #[cfg(windows)]
    assert!(
        unsafe { windows::Win32::System::Console::GetConsoleWindow() }
            .0
            .is_null()
    );
    assert!(std::env::args().any(|argument| argument == PROBE_ARGUMENT));
    let mut input = Vec::new();
    assert_eq!(std::io::stdin().read_to_end(&mut input).unwrap(), 0);
    if mode == "nested" {
        let mut command = Command::new(std::env::current_exe().unwrap());
        configure_probe(&mut command, "success");
        assert!(command.status().unwrap().success());
        return;
    }
    println!(
        "\n{}",
        serde_json::json!({ "type": "progress", "percent": 2, "remainingSeconds": 30 })
    );
    println!(
        "{}",
        serde_json::json!({ "type": "result", "text": "会议内容", "segments": [] })
    );
    eprintln!("worker diagnostic");
    if mode == "failure" {
        std::process::exit(23);
    }
}

fn run_probe(mode: &str) -> Output {
    let mut command = python_command(&std::env::current_exe().unwrap());
    configure_probe(&mut command, mode);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            panic!("background worker probe timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn configure_probe(command: &mut Command, mode: &str) {
    command.args([
        "--exact",
        PROBE_TEST,
        "--nocapture",
        "--skip",
        PROBE_ARGUMENT,
    ]);
    command.env(PROBE_MODE, mode);
}
