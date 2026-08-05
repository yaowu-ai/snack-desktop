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
const localSigningUnlockScript = path.join(
  repoRoot,
  "scripts",
  "unlock-local-signing-keychain.zsh"
);

dotenv.config({
  path: envPath,
  override: false,
});

const operationMap = {
  dev: { command: "dev", disableUpdater: true, localSigning: false },
  build: { command: "build", disableUpdater: false, localSigning: false },
  "build-local": { command: "build", disableUpdater: true, localSigning: true },
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

const LOCAL_MACOS_SIGNING_IDENTITY = "Snack Record Local Code Signing";

const resolveMacosSigningIdentity = (useLocalSigning) => {
  if (process.platform !== "darwin") {
    return "";
  }

  const configuredIdentity =
    process.env.SNACK_MACOS_SIGNING_IDENTITY?.trim() ||
    process.env.APPLE_SIGNING_IDENTITY?.trim();
  if (configuredIdentity) {
    return configuredIdentity;
  }

  if (!useLocalSigning) {
    return "";
  }

  const unlockResult = spawnSync("/bin/zsh", [localSigningUnlockScript], {
    encoding: "utf8",
  });
  if (unlockResult.status !== 0) {
    const detail = unlockResult.stderr?.trim() || unlockResult.error?.message;
    console.error("Unable to unlock the managed local signing keychain.");
    if (detail) console.error(detail);
    return "";
  }

  const result = spawnSync("security", ["find-identity", "-v", "-p", "codesigning"], {
    encoding: "utf8",
  });

  if (
    result.status === 0 &&
    result.stdout.includes(`"${LOCAL_MACOS_SIGNING_IDENTITY}"`)
  ) {
    return LOCAL_MACOS_SIGNING_IDENTITY;
  }

  return "";
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

const operationName = process.argv[2];
const operation = operationMap[operationName];

if (!operation) {
  console.error("Usage: node scripts/index.cjs <dev|build|build-local> [qa|prod] [...tauriArgs]");
  process.exit(1);
}

const args = process.argv.slice(3);
let targetEnv = (process.env.SNACK_ENV || inferEnvFromGitRef()).toLowerCase();

if (args[0] && !args[0].startsWith("-")) {
  targetEnv = args.shift().toLowerCase();
}

if (operation.localSigning && !args.includes("--debug") && !args.includes("-d")) {
  console.error("Local app builds must use --debug. Run npm run build:local.");
  process.exit(1);
}

if (!(targetEnv in updaterEndpointMap)) {
  console.error(`Unknown ${operation.command} environment: ${targetEnv}`);
  console.error(`Supported environments: ${Object.keys(updaterEndpointMap).join(", ")}`);
  process.exit(1);
}

const configuredHost = envValue("SNACK_HOST", "");
if (!configuredHost) {
  console.error("Missing SNACK_HOST. Set it in the command environment or the local .env file.");
  process.exit(1);
}

const normalizeFrontendUrl = (hostOrUrl) =>
  /^https?:\/\//i.test(hostOrUrl) ? hostOrUrl : `https://${hostOrUrl}`;
const frontendUrl = normalizeFrontendUrl(configuredHost);
const updaterEndpoint = operation.disableUpdater ? null : updaterEndpointMap[targetEnv];

try {
  const parsedFrontendUrl = new URL(frontendUrl);
  if (!["http:", "https:"].includes(parsedFrontendUrl.protocol)) {
    throw new Error(`unsupported protocol ${parsedFrontendUrl.protocol}`);
  }
} catch (error) {
  console.error(`Invalid frontend URL for ${targetEnv}: ${frontendUrl}`);
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
}

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
const macosSigningIdentity = resolveMacosSigningIdentity(operation.localSigning);

if (operation.localSigning && process.platform === "darwin" && !macosSigningIdentity) {
  console.error(
    `Missing macOS signing identity "${LOCAL_MACOS_SIGNING_IDENTITY}". ` +
      "Set SNACK_MACOS_SIGNING_IDENTITY or install the managed local identity first."
  );
  process.exit(1);
}

if (operation.command === "build" && !operation.localSigning && !updaterPubkey) {
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
    ...(macosSigningIdentity
      ? { macOS: { signingIdentity: macosSigningIdentity } }
      : {}),
  },
  plugins: {
    updater: {
      endpoints: updaterEndpoint ? [updaterEndpoint] : [],
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

if (macosSigningIdentity) {
  childEnv.APPLE_SIGNING_IDENTITY = macosSigningIdentity;
}

if (process.env.SNACK_DESKTOP_BASE_UA) {
  childEnv.SNACK_DESKTOP_BASE_UA = process.env.SNACK_DESKTOP_BASE_UA;
}

const child = spawn(tauriBin, [operation.command, ...args], {
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
