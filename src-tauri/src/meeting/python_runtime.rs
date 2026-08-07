//! Snack-owned Python lifecycle for local transcription.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};

const RUNTIME_ID: &str = "cpython-3.12.13+20260718";
const PYTHON_VERSION: (u8, u8, u8) = (3, 12, 13);
const REQUIREMENTS: &str = include_str!("requirements.lock.txt");
const STATE_FILE: &str = ".runtime-state.json";
const LEGACY_READY_FILE: &str = ".requirements-installed";
const PIP_LOG_FILE: &str = "pip-install.log";
const PIP_ATTEMPTS: u8 = 2;
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(250);
const VENV_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const PIP_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const MAX_DIAGNOSTIC_LINES: usize = 120;
const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const PYTHON_INFO_SCRIPT: &str = concat!(
    "import json,platform,sys;",
    "print(json.dumps({'major':sys.version_info.major,'minor':sys.version_info.minor,",
    "'patch':sys.version_info.micro,'machine':platform.machine()}))"
);
const DISK_PATTERNS: &[&str] = &["no space left", "errno 28", "disk full"];
const PERMISSION_PATTERNS: &[&str] = &["permission denied", "access is denied", "errno 13"];
const COMPATIBILITY_PATTERNS: &[&str] = &[
    "requires-python",
    "requires python",
    "no matching distribution found",
    "could not find a version that satisfies",
    "not a supported wheel",
];
const NETWORK_PATTERNS: &[&str] = &[
    "timed out",
    "temporary failure in name resolution",
    "connection",
    "network is unreachable",
    "name or service not known",
    "certificate verify",
    "sslerror",
    "proxyerror",
];

pub(crate) struct RuntimeSetup<'a> {
    pub(crate) app: &'a AppHandle,
    pub(crate) runtime_dir: &'a Path,
}

#[derive(Debug)]
struct ManagedPython {
    executable: PathBuf,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeManifest {
    runtime_id: String,
    python_version: String,
    target: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct PythonInfo {
    major: u8,
    minor: u8,
    patch: u8,
    machine: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RuntimeState {
    runtime_id: String,
    python_version: String,
    requirements_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureCategory {
    Package,
    Compatibility,
    Network,
    Disk,
    Permission,
    Timeout,
    Dependency,
}

impl FailureCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::Compatibility => "compatibility",
            Self::Network => "network",
            Self::Disk => "disk",
            Self::Permission => "permission",
            Self::Timeout => "timeout",
            Self::Dependency => "dependency",
        }
    }

    fn user_message(self) -> &'static str {
        match self {
            Self::Package => "Snack 本地运行环境缺失或损坏，请更新或重新安装 Snack 后重试",
            Self::Compatibility => "本地运行环境与转写依赖不兼容，请更新 Snack 后重试",
            Self::Network => "下载本地转写依赖失败，请检查网络或代理设置后重试",
            Self::Disk => "磁盘空间不足，无法安装本地转写依赖，请清理空间后重试",
            Self::Permission => "无法写入本地运行环境，请检查磁盘权限后重试",
            Self::Timeout => "本地转写依赖安装超时，请检查网络后重试",
            Self::Dependency => "本地转写依赖安装失败，请查看 Snack 日志后重试",
        }
    }
}

#[derive(Debug)]
struct RuntimeFailure {
    category: FailureCategory,
    diagnostic: String,
    exit_code: Option<i32>,
    attempts: u8,
}

impl RuntimeFailure {
    fn package(diagnostic: impl Into<String>) -> Self {
        Self {
            category: FailureCategory::Package,
            diagnostic: diagnostic.into(),
            exit_code: None,
            attempts: 0,
        }
    }
}

struct CommandOutcome {
    success: bool,
    timed_out: bool,
    exit_code: Option<i32>,
    diagnostic: String,
}

pub(crate) fn ensure_ready(params: RuntimeSetup<'_>) -> Result<PathBuf, String> {
    match ensure_ready_inner(&params) {
        Ok(executable) => Ok(executable),
        Err(failure) => {
            log_failure(params.app, &failure);
            Err(failure.category.user_message().to_string())
        }
    }
}

fn ensure_ready_inner(params: &RuntimeSetup<'_>) -> Result<PathBuf, RuntimeFailure> {
    fs::create_dir_all(params.runtime_dir)
        .map_err(|error| failure_from_io("create runtime directory", error))?;
    fs::write(
        params.runtime_dir.join("requirements.lock.txt"),
        REQUIREMENTS,
    )
    .map_err(|error| failure_from_io("write dependency lock", error))?;
    fs::remove_file(params.runtime_dir.join("requirements.txt")).ok();
    fs::remove_file(params.runtime_dir.join(LEGACY_READY_FILE)).ok();
    let managed = locate_managed_python(params.app)?;
    let expected = expected_state(&managed);
    if !environment_is_current(params.runtime_dir, &expected) {
        rebuild_environment(params.runtime_dir, &managed)?;
        install_dependencies(params.runtime_dir)?;
        write_state(params.runtime_dir, &expected)?;
    }
    Ok(venv_python(params.runtime_dir))
}

fn locate_managed_python(app: &AppHandle) -> Result<ManagedPython, RuntimeFailure> {
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|error| RuntimeFailure::package(format!("resolve resource directory: {error}")))?;
    let root = resource_dir.join("python-runtime");
    validate_runtime_manifest(&root)?;
    let executable = bundled_python(&root);
    let info = inspect_python(&executable)?;
    validate_python_info(&info)?;
    Ok(ManagedPython {
        executable,
        version: format!("{}.{}.{}", info.major, info.minor, info.patch),
    })
}

fn validate_runtime_manifest(root: &Path) -> Result<(), RuntimeFailure> {
    let path = root.join("runtime-manifest.json");
    let bytes = fs::read(&path)
        .map_err(|error| RuntimeFailure::package(format!("read {}: {error}", path.display())))?;
    let manifest: RuntimeManifest = serde_json::from_slice(&bytes)
        .map_err(|error| RuntimeFailure::package(format!("parse runtime manifest: {error}")))?;
    if manifest.runtime_id != RUNTIME_ID || manifest.python_version != version_string() {
        return Err(RuntimeFailure::package(format!(
            "unexpected runtime manifest: {} {}",
            manifest.runtime_id, manifest.python_version
        )));
    }
    if !manifest.target.contains(expected_target_family()) {
        return Err(RuntimeFailure::package(format!(
            "runtime target mismatch: {}",
            manifest.target
        )));
    }
    if manifest.sha256 != expected_runtime_sha256() {
        return Err(RuntimeFailure::package("runtime asset digest mismatch"));
    }
    Ok(())
}

fn inspect_python(executable: &Path) -> Result<PythonInfo, RuntimeFailure> {
    if !executable.is_file() {
        return Err(RuntimeFailure::package(format!(
            "managed interpreter missing: {}",
            executable.display()
        )));
    }
    let output = Command::new(executable)
        .args(["-I", "-c", PYTHON_INFO_SCRIPT])
        .output()
        .map_err(|error| RuntimeFailure::package(format!("start managed interpreter: {error}")))?;
    decode_python_info(output)
}

fn decode_python_info(output: std::process::Output) -> Result<PythonInfo, RuntimeFailure> {
    if !output.status.success() {
        return Err(RuntimeFailure::package(format!(
            "managed interpreter exited {:?}: {}",
            output.status.code(),
            sanitize_diagnostic(&String::from_utf8_lossy(&output.stderr))
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| RuntimeFailure::package(format!("read managed Python version: {error}")))
}

fn validate_python_info(info: &PythonInfo) -> Result<(), RuntimeFailure> {
    if (info.major, info.minor, info.patch) != PYTHON_VERSION {
        return Err(RuntimeFailure::package(format!(
            "managed Python version mismatch: {}.{}.{}",
            info.major, info.minor, info.patch
        )));
    }
    if normalized_arch(&info.machine) != expected_arch() {
        return Err(RuntimeFailure::package(format!(
            "managed Python architecture mismatch: {}",
            info.machine
        )));
    }
    Ok(())
}

fn expected_state(managed: &ManagedPython) -> RuntimeState {
    RuntimeState {
        runtime_id: RUNTIME_ID.to_string(),
        python_version: managed.version.clone(),
        requirements_sha256: requirements_sha256(),
    }
}

fn environment_is_current(runtime_dir: &Path, expected: &RuntimeState) -> bool {
    let state_path = runtime_dir.join(STATE_FILE);
    let Ok(bytes) = fs::read(state_path) else {
        return false;
    };
    let Ok(saved) = serde_json::from_slice::<RuntimeState>(&bytes) else {
        return false;
    };
    if &saved != expected {
        return false;
    }
    inspect_python(&venv_python(runtime_dir)).is_ok() && dependencies_import(runtime_dir)
}

fn dependencies_import(runtime_dir: &Path) -> bool {
    Command::new(venv_python(runtime_dir))
        .args([
            "-I",
            "-c",
            "import funasr,librosa,modelscope,soundfile,torch,torchaudio",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn rebuild_environment(runtime_dir: &Path, managed: &ManagedPython) -> Result<(), RuntimeFailure> {
    let venv = runtime_dir.join("venv");
    fs::remove_file(runtime_dir.join(STATE_FILE)).ok();
    if venv.exists() {
        fs::remove_dir_all(&venv)
            .map_err(|error| failure_from_io("remove stale virtual environment", error))?;
    }
    let mut command = Command::new(&managed.executable);
    command.args(["-I", "-m", "venv", "--copies"]).arg(&venv);
    let outcome = run_logged_command(
        &mut command,
        &runtime_dir.join("venv-create.log"),
        VENV_TIMEOUT,
    )?;
    if outcome.success {
        return Ok(());
    }
    Err(failure_from_outcome(outcome, 1))
}

fn install_dependencies(runtime_dir: &Path) -> Result<(), RuntimeFailure> {
    for attempt in 1..=PIP_ATTEMPTS {
        let outcome = run_pip(runtime_dir)?;
        if outcome.success {
            return Ok(());
        }
        let failure = failure_from_outcome(outcome, attempt);
        if attempt == PIP_ATTEMPTS || !is_retryable(failure.category) {
            return Err(failure);
        }
        thread::sleep(Duration::from_secs(1 << attempt));
    }
    unreachable!("bounded pip attempts always return")
}

fn run_pip(runtime_dir: &Path) -> Result<CommandOutcome, RuntimeFailure> {
    let mut command = Command::new(venv_python(runtime_dir));
    command.args([
        "-I",
        "-m",
        "pip",
        "install",
        "--disable-pip-version-check",
        "--prefer-binary",
        "--timeout=60",
        "--retries=3",
        "--requirement",
    ]);
    command.arg(runtime_dir.join("requirements.lock.txt"));
    command.env("PIP_NO_INPUT", "1");
    command.env("PIP_CACHE_DIR", runtime_dir.join("pip-cache"));
    command.env("PYTHONUTF8", "1");
    run_logged_command(&mut command, &runtime_dir.join(PIP_LOG_FILE), PIP_TIMEOUT)
}

fn run_logged_command(
    command: &mut Command,
    log_path: &Path,
    timeout: Duration,
) -> Result<CommandOutcome, RuntimeFailure> {
    let stdout =
        File::create(log_path).map_err(|error| failure_from_io("create diagnostic log", error))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| failure_from_io("open diagnostic log", error))?;
    let mut child = command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| failure_from_io("start local runtime command", error))?;
    let started = Instant::now();
    let (status, timed_out) = wait_for_child(&mut child, started, timeout)?;
    let diagnostic = sanitize_log_file(log_path);
    Ok(CommandOutcome {
        success: status.is_some_and(|value| value.success()) && !timed_out,
        timed_out,
        exit_code: status.and_then(|value| value.code()),
        diagnostic,
    })
}

fn wait_for_child(
    child: &mut std::process::Child,
    started: Instant,
    timeout: Duration,
) -> Result<(Option<std::process::ExitStatus>, bool), RuntimeFailure> {
    loop {
        let status = child
            .try_wait()
            .map_err(|error| failure_from_io("wait for local runtime command", error))?;
        if status.is_some() {
            return Ok((status, false));
        }
        if started.elapsed() >= timeout {
            child.kill().ok();
            let status = child.wait().ok();
            return Ok((status, true));
        }
        thread::sleep(COMMAND_POLL_INTERVAL);
    }
}

fn failure_from_outcome(outcome: CommandOutcome, attempts: u8) -> RuntimeFailure {
    RuntimeFailure {
        category: classify_failure(&outcome.diagnostic, outcome.timed_out),
        diagnostic: outcome.diagnostic,
        exit_code: outcome.exit_code,
        attempts,
    }
}

fn failure_from_io(action: &str, error: std::io::Error) -> RuntimeFailure {
    let category = match error.kind() {
        std::io::ErrorKind::PermissionDenied => FailureCategory::Permission,
        std::io::ErrorKind::StorageFull => FailureCategory::Disk,
        _ => FailureCategory::Package,
    };
    RuntimeFailure {
        category,
        diagnostic: format!("{action}: {error}"),
        exit_code: None,
        attempts: 0,
    }
}

fn classify_failure(diagnostic: &str, timed_out: bool) -> FailureCategory {
    if timed_out {
        return FailureCategory::Timeout;
    }
    let detail = diagnostic.to_ascii_lowercase();
    classify_diagnostic(&detail)
}

fn classify_diagnostic(detail: &str) -> FailureCategory {
    [
        (FailureCategory::Disk, DISK_PATTERNS),
        (FailureCategory::Permission, PERMISSION_PATTERNS),
        (FailureCategory::Compatibility, COMPATIBILITY_PATTERNS),
        (FailureCategory::Network, NETWORK_PATTERNS),
    ]
    .into_iter()
    .find_map(|(category, patterns)| contains_any(detail, patterns).then_some(category))
    .unwrap_or(FailureCategory::Dependency)
}

fn contains_any(value: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| value.contains(pattern))
}

fn is_retryable(category: FailureCategory) -> bool {
    matches!(
        category,
        FailureCategory::Network | FailureCategory::Timeout
    )
}

fn write_state(runtime_dir: &Path, state: &RuntimeState) -> Result<(), RuntimeFailure> {
    let path = runtime_dir.join(STATE_FILE);
    let temporary = runtime_dir.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| RuntimeFailure::package(format!("serialize runtime state: {error}")))?;
    fs::write(&temporary, bytes).map_err(|error| failure_from_io("write runtime state", error))?;
    fs::remove_file(&path).ok();
    fs::rename(&temporary, &path).map_err(|error| failure_from_io("save runtime state", error))
}

fn log_failure(app: &AppHandle, failure: &RuntimeFailure) {
    crate::logging::write_app_log(
        app,
        "error",
        "meeting-model",
        "local Python runtime setup failed",
        Some(&serde_json::json!({
            "category": failure.category.as_str(),
            "runtimeId": RUNTIME_ID,
            "exitCode": failure.exit_code,
            "attempts": failure.attempts,
            "diagnostic": failure.diagnostic,
        })),
    );
}

pub(crate) fn model_download_failure(
    app: &AppHandle,
    log_path: &Path,
    exit_code: Option<i32>,
) -> String {
    let diagnostic = sanitize_log_file(log_path);
    let mut failure = failure_from_outcome(
        CommandOutcome {
            success: false,
            timed_out: false,
            exit_code,
            diagnostic,
        },
        1,
    );
    if failure.category == FailureCategory::Dependency {
        failure.diagnostic = format!("ModelScope downloader failed\n{}", failure.diagnostic);
    }
    log_failure(app, &failure);
    model_download_message(failure.category).to_string()
}

fn model_download_message(category: FailureCategory) -> &'static str {
    match category {
        FailureCategory::Network | FailureCategory::Timeout => {
            "模型下载失败，请检查网络或代理设置后重试"
        }
        FailureCategory::Disk => "磁盘空间不足，模型下载未完成，请清理空间后重试",
        FailureCategory::Permission => "无法写入模型文件，请检查磁盘权限后重试",
        _ => "模型下载失败，请查看 Snack 日志后重试",
    }
}

pub(crate) fn sanitize_download_log(path: &Path) {
    sanitize_log_file(path);
}

fn sanitize_log_file(path: &Path) -> String {
    let raw = read_log_tail(path);
    let sanitized = sanitize_diagnostic(&String::from_utf8_lossy(&raw));
    fs::write(path, sanitized.as_bytes()).ok();
    sanitized
}

fn read_log_tail(path: &Path) -> Vec<u8> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = length.saturating_sub((MAX_DIAGNOSTIC_BYTES * 4) as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok();
    bytes
}

fn sanitize_diagnostic(value: &str) -> String {
    let lines = value.lines().map(sanitize_line).collect::<Vec<_>>();
    let start = lines.len().saturating_sub(MAX_DIAGNOSTIC_LINES);
    let joined = lines[start..].join("\n");
    trim_to_last_bytes(&joined, MAX_DIAGNOSTIC_BYTES)
}

fn sanitize_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let contains_url = lower.contains("://");
    let exposes_secret = contains_any(
        &lower,
        &["authorization:", "password=", "passwd=", "api_key="],
    ) || (!contains_url && lower.contains("token="));
    if exposes_secret {
        return "[redacted sensitive diagnostic line]".to_string();
    }
    line.split_whitespace()
        .map(sanitize_url_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_url_token(token: &str) -> String {
    let Some(scheme_at) = token.find("://") else {
        return token.to_string();
    };
    let authority_start = scheme_at + 3;
    let suffix = &token[authority_start..];
    let authority_end = suffix.find('/').unwrap_or(suffix.len());
    let authority = &suffix[..authority_end];
    let safe_authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let mut safe = format!("{}{}", &token[..authority_start], safe_authority);
    safe.push_str(&suffix[authority_end..]);
    let secret_at = safe.find(['?', '#']).unwrap_or(safe.len());
    safe.truncate(secret_at);
    safe
}

fn trim_to_last_bytes(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut start = value.len() - limit;
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("[diagnostic truncated]\n{}", &value[start..])
}

fn requirements_sha256() -> String {
    format!("{:x}", Sha256::digest(REQUIREMENTS.as_bytes()))
}

fn version_string() -> String {
    format!(
        "{}.{}.{}",
        PYTHON_VERSION.0, PYTHON_VERSION.1, PYTHON_VERSION.2
    )
}

fn bundled_python(root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        root.join("python/python.exe")
    } else {
        root.join("python/bin/python3")
    }
}

pub(crate) fn venv_python(runtime_dir: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        runtime_dir.join("venv/Scripts/python.exe")
    } else {
        runtime_dir.join("venv/bin/python")
    }
}

fn expected_target_family() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else {
        "apple-darwin"
    }
}

fn expected_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86_64"
    }
}

fn expected_runtime_sha256() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "9a1e9e06175c10efd8378b904b07fa21bd791ab3345d7cdffeb4a76c9ff55903"
    } else if cfg!(target_os = "macos") {
        "8e6b7e6533bdf746287008edf91102e7bee0a6ca1d24f16c4514237cafd706c5"
    } else {
        "0d422a1439ec308e03f47df551bc30f5994727c456e414b026d202bcda9b7c1c"
    }
}

fn normalized_arch(value: &str) -> &'static str {
    match value.to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "arm64",
        "amd64" | "x86_64" => "x86_64",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_actionable_failures() {
        assert_eq!(
            classify_failure("No matching distribution found for modelscope", false),
            FailureCategory::Compatibility
        );
        assert_eq!(
            classify_failure("Temporary failure in name resolution", false),
            FailureCategory::Network
        );
        assert_eq!(
            classify_failure("OSError: [Errno 28] No space left", false),
            FailureCategory::Disk
        );
        assert_eq!(classify_failure("", true), FailureCategory::Timeout);
    }

    #[test]
    fn redacts_credentials_and_query_parameters() {
        let raw = concat!(
            "GET https://alice:secret@example.com/simple?token=abc\n",
            "Authorization: Bearer secret\n",
        );
        let sanitized = sanitize_diagnostic(raw);
        assert!(sanitized.contains("https://example.com/simple"));
        assert!(!sanitized.contains("alice"));
        assert!(!sanitized.contains("secret"));
        assert!(!sanitized.contains("token"));
    }

    #[test]
    fn runtime_state_changes_with_dependency_lock() {
        let state = RuntimeState {
            runtime_id: RUNTIME_ID.to_string(),
            python_version: version_string(),
            requirements_sha256: requirements_sha256(),
        };
        assert_eq!(state.runtime_id, "cpython-3.12.13+20260718");
        assert_eq!(state.requirements_sha256.len(), 64);
    }

    #[test]
    fn resolves_cross_platform_venv_paths() {
        let path = venv_python(Path::new("runtime"));
        if cfg!(target_os = "windows") {
            assert!(path.ends_with("venv/Scripts/python.exe"));
        } else {
            assert!(path.ends_with("venv/bin/python"));
        }
    }
}
