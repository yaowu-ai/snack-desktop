#!/usr/bin/env node

const crypto = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const RELEASE = "20260718";
const PYTHON_VERSION = "3.12.13";
const RUNTIME_ID = `cpython-${PYTHON_VERSION}+${RELEASE}`;
const RELEASE_BASE =
  `https://github.com/astral-sh/python-build-standalone/releases/download/${RELEASE}`;
const repoRoot = path.resolve(__dirname, "..");
const outputRoot = path.join(repoRoot, "src-tauri", "resources", "python-runtime");
const cacheRoot = path.join(repoRoot, "src-tauri", "target", "python-runtime-cache");

const assets = {
  "darwin-arm64": {
    target: "aarch64-apple-darwin",
    sha256: "9a1e9e06175c10efd8378b904b07fa21bd791ab3345d7cdffeb4a76c9ff55903",
  },
  "darwin-x64": {
    target: "x86_64-apple-darwin",
    sha256: "8e6b7e6533bdf746287008edf91102e7bee0a6ca1d24f16c4514237cafd706c5",
  },
  "win32-x64": {
    target: "x86_64-pc-windows-msvc",
    sha256: "0d422a1439ec308e03f47df551bc30f5994727c456e414b026d202bcda9b7c1c",
  },
};

function targetKey() {
  const triple = (
    process.env.SNACK_TARGET_TRIPLE ||
    process.env.CARGO_BUILD_TARGET ||
    process.env.TAURI_ENV_TARGET_TRIPLE
  )?.trim();
  if (!triple) return `${process.platform}-${process.arch}`;
  if (triple.includes("apple-darwin")) {
    return triple.startsWith("aarch64") ? "darwin-arm64" : "darwin-x64";
  }
  if (triple.includes("windows-msvc") && triple.startsWith("x86_64")) {
    return "win32-x64";
  }
  return triple;
}

function resolveAsset() {
  const key = targetKey();
  const asset = assets[key];
  if (!asset) throw new Error(`Unsupported Snack Python runtime target: ${key}`);
  const filename =
    `${RUNTIME_ID}-${asset.target}-install_only_stripped.tar.gz`;
  return { ...asset, filename, url: `${RELEASE_BASE}/${filename.replace("+", "%2B")}` };
}

function sha256(filePath) {
  const hash = crypto.createHash("sha256");
  const bytes = fs.readFileSync(filePath);
  return hash.update(bytes).digest("hex");
}

function download(url, destination, redirects = 0) {
  if (redirects > 8) return Promise.reject(new Error("Too many download redirects"));
  return new Promise((resolve, reject) => {
    const request = https.get(url, { timeout: 30_000 }, (response) => {
      if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
        response.resume();
        resolve(download(new URL(response.headers.location, url).toString(), destination, redirects + 1));
        return;
      }
      if (response.statusCode !== 200) {
        response.resume();
        reject(new Error(`Python runtime download failed: HTTP ${response.statusCode}`));
        return;
      }
      const file = fs.createWriteStream(destination);
      response.pipe(file);
      file.on("finish", () => file.close(resolve));
      file.on("error", reject);
    });
    request.on("timeout", () => request.destroy(new Error("Python runtime download timed out")));
    request.on("error", reject);
  });
}

async function ensureArchive(asset) {
  fs.mkdirSync(cacheRoot, { recursive: true });
  const archive = path.join(cacheRoot, asset.filename);
  const localArchive = process.env.SNACK_PYTHON_RUNTIME_ARCHIVE?.trim();
  if (localArchive) fs.copyFileSync(path.resolve(localArchive), archive);
  if (!fs.existsSync(archive) || sha256(archive) !== asset.sha256) {
    fs.rmSync(archive, { force: true });
    console.log(`Downloading managed Python ${PYTHON_VERSION} for ${asset.target}...`);
    await download(asset.url, archive);
  }
  const actual = sha256(archive);
  if (actual !== asset.sha256) {
    fs.rmSync(archive, { force: true });
    throw new Error(`Python runtime SHA-256 mismatch: expected ${asset.sha256}, got ${actual}`);
  }
  return archive;
}

function extractRuntime(archive, asset) {
  const temporary = fs.mkdtempSync(path.join(cacheRoot, "extract-"));
  const result = spawnSync("tar", ["-xzf", archive, "-C", temporary], {
    encoding: "utf8",
  });
  if (result.status !== 0) {
    fs.rmSync(temporary, { recursive: true, force: true });
    throw new Error(result.stderr?.trim() || "Unable to extract Python runtime");
  }
  const extracted = validatedRuntimePath(temporary, asset);
  installExtractedRuntime(extracted, temporary, asset);
}

function validatedRuntimePath(temporary, asset) {
  const extracted = path.join(temporary, "python");
  const executableName = asset.target.includes("windows") ? "python.exe" : "bin/python3";
  if (!fs.existsSync(path.join(extracted, executableName))) {
    fs.rmSync(temporary, { recursive: true, force: true });
    throw new Error("Downloaded archive does not contain the managed Python executable");
  }
  return extracted;
}

function installExtractedRuntime(extracted, temporary, asset) {
  fs.mkdirSync(outputRoot, { recursive: true });
  fs.rmSync(path.join(outputRoot, "python"), { recursive: true, force: true });
  fs.renameSync(extracted, path.join(outputRoot, "python"));
  fs.rmSync(temporary, { recursive: true, force: true });
  fs.writeFileSync(
    path.join(outputRoot, "runtime-manifest.json"),
    `${JSON.stringify({ runtimeId: RUNTIME_ID, pythonVersion: PYTHON_VERSION, target: asset.target, sha256: asset.sha256 }, null, 2)}\n`,
  );
}

function runtimeIsCurrent(asset) {
  const manifestPath = path.join(outputRoot, "runtime-manifest.json");
  if (!fs.existsSync(manifestPath)) return false;
  try {
    const current = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    const executableName = asset.target.includes("windows") ? "python.exe" : "bin/python3";
    const executable = path.join(outputRoot, "python", executableName);
    return current.runtimeId === RUNTIME_ID &&
      current.target === asset.target &&
      current.sha256 === asset.sha256 &&
      fs.existsSync(executable);
  } catch {
    return false;
  }
}

async function main() {
  const asset = resolveAsset();
  if (process.argv.includes("--print-manifest")) {
    console.log(JSON.stringify({ runtimeId: RUNTIME_ID, ...asset }, null, 2));
    return;
  }
  if (runtimeIsCurrent(asset)) return;
  extractRuntime(await ensureArchive(asset), asset);
  console.log(`Prepared ${RUNTIME_ID} for ${asset.target}.`);
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
