#!/usr/bin/env node

const { spawn } = require("node:child_process");
const path = require("node:path");
const dotenv = require("dotenv");

const repoRoot = path.resolve(__dirname, "..");
const envPath = path.join(repoRoot, ".env");
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
  prod: envValue("SNACK_PROD_HOST", "snack.mechlabs.cn"),
  qa: envValue("SNACK_QA_HOST", "qasnack.mechlabs.cn"),
};

const updaterEndpointMap = {
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
  console.error("Usage: node scripts/index.cjs <dev|build> [qa|prod] [...tauriArgs]");
  process.exit(1);
}

const args = process.argv.slice(3);
let targetEnv = (process.env.SNACK_ENV || inferEnvFromGitRef()).toLowerCase();

if (args[0] && !args[0].startsWith("-")) {
  targetEnv = args.shift().toLowerCase();
}

const host = hostMap[targetEnv];
const updaterEndpoint = updaterEndpointMap[targetEnv];

if (!host || !updaterEndpoint) {
  console.error(`Unknown ${command} environment: ${targetEnv}`);
  console.error(`Supported environments: ${Object.keys(hostMap).join(", ")}`);
  process.exit(1);
}

const frontendUrl = `https://${host}`;
const normalizeUpdaterPubkey = (value) => {
  const pubkey = value?.trim();
  if (!pubkey) {
    return "";
  }

  if (pubkey.includes("\n")) {
    return pubkey;
  }

  return `untrusted comment: minisign public key ${pubkey.slice(0, 16)}\n${pubkey}`;
};

const updaterPubkey = normalizeUpdaterPubkey(
  process.env.TAURI_UPDATER_PUBKEY || process.env.TAURI_PUBLIC_KEY
);
const createUpdaterArtifacts = process.env.SNACK_CREATE_UPDATER_ARTIFACTS !== "false";

if (command === "build" && createUpdaterArtifacts && !updaterPubkey) {
  console.error(
    "Missing TAURI_UPDATER_PUBKEY. Generate an updater keypair with `tauri signer generate`, then set the public key before building."
  );
  process.exit(1);
}

const tauriConfig = {
  build: {
    devUrl: frontendUrl,
    frontendDist: frontendUrl,
  },
  bundle: {
    createUpdaterArtifacts,
  },
  plugins: {
    updater: {
      endpoints: [updaterEndpoint],
      ...(createUpdaterArtifacts && updaterPubkey ? { pubkey: updaterPubkey } : {}),
    },
  },
};

const childEnv = {
  ...process.env,
  TAURI_CONFIG: JSON.stringify(tauriConfig),
  SNACK_ENV: targetEnv,
  SNACK_FRONTEND_URL: frontendUrl,
};

if (process.env.SNACK_DESKTOP_BASE_UA) {
  childEnv.SNACK_DESKTOP_BASE_UA = process.env.SNACK_DESKTOP_BASE_UA;
}

const child = spawn(tauriBin, [command, ...args], {
  cwd: repoRoot,
  stdio: "inherit",
  env: childEnv,
  shell: process.platform === "win32",
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }

  process.exit(code ?? 1);
});
