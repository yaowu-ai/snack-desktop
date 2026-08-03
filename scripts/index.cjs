#!/usr/bin/env node

const { spawn, spawnSync } = require("node:child_process");
const path = require("node:path");
const dotenv = require("dotenv");

const repoRoot = path.resolve(__dirname, "..");
const envPath = path.join(repoRoot, ".env");
const tauriConfPath = path.join(repoRoot, "src-tauri", "tauri.conf.json");
const tauriBin = path.join(
  repoRoot,
  "node_modules",
  ".bin",
  process.platform === "win32" ? "tauri.cmd" : "tauri"
);

dotenv.config({
  path: envPath,
});

const commandMap = {
  dev: "dev",
  build: "build",
};

const inferEnvFromGitRef = () => {
  const ref = process.env.GITHUB_REF || "";
  const refName = process.env.GITHUB_REF_NAME || "";
  const refType = process.env.GITHUB_REF_TYPE || "";

  if (refType === "tag" || ref.startsWith("refs/tags/")) {
    return "prod";
  }

  if (refName === "test" || ref === "refs/heads/test") {
    return "qa";
  }

  if (refName === "prod" || ref === "refs/heads/prod") {
    return "prod";
  }

  return "prod";
};

const envValue = (name, fallback) => {
  const value = process.env[name]?.trim();
  return value || fallback;
};

const hostMap = {
  local: null,
  prod: envValue("SNACK_PROD_HOST", "snack.mechlabs.cn"),
  qa: envValue("SNACK_QA_HOST", "qasnack.mechlabs.cn"),
};

const updaterEndpointMap = {
  local: envValue(
    "SNACK_PROD_UPDATER_ENDPOINT",
    "https://snack.mechlabs.cn/api/desktop-updates/update?currentVersion={{current_version}}&target={{target}}&arch={{arch}}",
  ),
  prod: envValue(
    "SNACK_PROD_UPDATER_ENDPOINT",
    "https://snack.mechlabs.cn/api/desktop-updates/update?currentVersion={{current_version}}&target={{target}}&arch={{arch}}",
  ),
  qa: envValue(
    "SNACK_QA_UPDATER_ENDPOINT",
    "https://qasnack.mechlabs.cn/api/desktop-updates/update?currentVersion={{current_version}}&target={{target}}&arch={{arch}}",
  ),
};

const command = commandMap[process.argv[2]];

if (!command) {
  console.error("Usage: node scripts/index.cjs <dev|build> [local|qa|prod] [...tauriArgs]");
  process.exit(1);
}

const args = process.argv.slice(3);
let targetEnv = (process.env.SNACK_ENV || inferEnvFromGitRef()).toLowerCase();

if (args[0] && !args[0].startsWith("-")) {
  targetEnv = args.shift().toLowerCase();
}

const updaterEndpoint = updaterEndpointMap[targetEnv];

if (!(targetEnv in hostMap) || !updaterEndpoint) {
  console.error(`Unknown ${command} environment: ${targetEnv}`);
  console.error(`Supported environments: ${Object.keys(hostMap).join(", ")}`);
  process.exit(1);
}

const frontendUrl =
  targetEnv === "local"
    ? envValue("SNACK_LOCAL_FRONTEND_URL", "http://localhost:3000")
    : `https://${hostMap[targetEnv]}`;
const normalizeUpdaterPubkey = (value) => {
  const pubkey = value?.trim();
  if (!pubkey) {
    return "";
  }

  const decodedPubkey = Buffer.from(pubkey, "base64").toString("utf8");
  if (decodedPubkey.startsWith("untrusted comment:")) {
    return pubkey;
  }

  if (pubkey.startsWith("untrusted comment:")) {
    return Buffer.from(pubkey, "utf8").toString("base64");
  }

  const barePubkey =
    decodedPubkey.startsWith("RW") && !decodedPubkey.includes("\n") ? decodedPubkey : pubkey;

  const minisignPubkey = barePubkey.includes("\n")
    ? barePubkey
    : `untrusted comment: minisign public key ${barePubkey.slice(0, 16)}\n${barePubkey}`;

  return Buffer.from(minisignPubkey, "utf8").toString("base64");
};

const updaterPubkey = normalizeUpdaterPubkey(
  process.env.TAURI_UPDATER_PUBKEY || process.env.TAURI_PUBLIC_KEY
);
const tauriConf = require(tauriConfPath);
const createUpdaterArtifacts =
  process.env.SNACK_CREATE_UPDATER_ARTIFACTS === "true" &&
  tauriConf.bundle?.createUpdaterArtifacts !== false;

const prepareEmbeddedRecordingRuntime = () => {
  if (
    command !== "build" ||
    process.platform !== "darwin" ||
    process.env.SNACK_EMBED_RECORDING_RUNTIME === "false"
  ) {
    return undefined;
  }

  const script = path.join(repoRoot, "scripts", "prepare-snack-record-runtime.sh");
  const result = spawnSync("bash", [script], {
    cwd: repoRoot,
    env: process.env,
    encoding: "utf8",
  });
  if (result.status !== 0) {
    process.stderr.write(result.stdout || "");
    process.stderr.write(result.stderr || "");
    process.exit(result.status || 1);
  }
  const runtimePath = result.stdout.trim().split("\n").filter(Boolean).at(-1);
  if (!runtimePath) {
    console.error("Snack Record runtime build did not return an app path.");
    process.exit(1);
  }
  return runtimePath;
};

const embeddedRecordingRuntimePath = prepareEmbeddedRecordingRuntime();

if (command === "build" && targetEnv !== "local" && !updaterPubkey) {
  console.error(
    "Missing TAURI_UPDATER_PUBKEY. Generate an updater keypair with `tauri signer generate`, then set the public key before building."
  );
  process.exit(1);
}

const tauriConfig = {
  ...(targetEnv === "local"
    ? {
        identifier: "cn.yaowutech.snack.record.local",
        productName: "Snack Record Local",
      }
    : {}),
  build: {
    devUrl: frontendUrl,
    frontendDist: frontendUrl,
  },
  bundle: {
    createUpdaterArtifacts,
    ...(embeddedRecordingRuntimePath
      ? {
          resources: {
            [embeddedRecordingRuntimePath]: "Snack Recording Service.app",
          },
        }
      : {}),
  },
  plugins: {
    ...(targetEnv === "local"
      ? {
          "deep-link": {
            desktop: { schemes: ["snack-record-local"] },
          },
        }
      : {}),
    updater: {
      endpoints: [updaterEndpoint],
      ...(updaterPubkey ? { pubkey: updaterPubkey } : {}),
    },
  },
};

const childEnv = {
  ...process.env,
  TAURI_CONFIG: JSON.stringify(tauriConfig),
  TAURI_UPDATER_PUBKEY: updaterPubkey,
  SNACK_ENV: targetEnv,
  SNACK_FRONTEND_URL: frontendUrl,
};

const tauriArgs = [
  command,
  ...(targetEnv === "local" ? ["--config", JSON.stringify(tauriConfig)] : []),
  ...args,
];

if (process.env.SNACK_DESKTOP_BASE_UA) {
  childEnv.SNACK_DESKTOP_BASE_UA = process.env.SNACK_DESKTOP_BASE_UA;
}

const child = spawn(tauriBin, tauriArgs, {
  cwd: repoRoot,
  stdio: "inherit",
  env: childEnv,
  shell: process.platform === "win32",
});

const finalizeMacOSBundle = () => {
  if (
    command !== "build" ||
    process.platform !== "darwin" ||
    !embeddedRecordingRuntimePath ||
    args.includes("--no-sign")
  ) {
    return true;
  }
  const targetIndex = args.indexOf("--target");
  const target = targetIndex >= 0 ? args[targetIndex + 1] : undefined;
  const bundleRoot = target
    ? path.join(repoRoot, "src-tauri", "target", target, "release", "bundle", "macos")
    : path.join(repoRoot, "src-tauri", "target", "release", "bundle", "macos");
  const productName = targetEnv === "local" ? "Snack Record Local" : tauriConf.productName;
  const appPath = path.join(bundleRoot, `${productName}.app`);
  const signingIdentity = process.env.APPLE_SIGNING_IDENTITY || process.env.SIGN_IDENTITY || "-";
  const sign = spawnSync(
    "codesign",
    [
      "--force",
      "--options",
      "runtime",
      "--sign",
      signingIdentity,
      "--entitlements",
      path.join(repoRoot, "src-tauri", "Entitlements.plist"),
      appPath,
    ],
    { cwd: repoRoot, encoding: "utf8" },
  );
  if (sign.status !== 0) {
    process.stderr.write(sign.stdout || "");
    process.stderr.write(sign.stderr || "");
    return false;
  }
  const verify = spawnSync("codesign", ["--verify", "--deep", "--strict", "--verbose=2", appPath], {
    cwd: repoRoot,
    encoding: "utf8",
  });
  if (verify.status !== 0) {
    process.stderr.write(verify.stdout || "");
    process.stderr.write(verify.stderr || "");
    return false;
  }
  return true;
};

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }

  if (code === 0 && !finalizeMacOSBundle()) {
    process.exit(1);
  }
  process.exit(code ?? 1);
});
