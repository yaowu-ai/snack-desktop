const { createHash } = require('node:crypto');
const { readFile, readdir } = require('node:fs/promises');
const path = require('node:path');
const { isDeepStrictEqual } = require('node:util');

const resourceSource = 'h-snack-website/src/lib/i18n';

/** Validate an offline snapshot, optionally against the Website release being delivered. */
async function verifySnapshot({ directory, release }) {
  // 1. Read and validate the artifact contract.
  const manifest = await readManifest(directory);
  // 2. Check an explicitly selected Website release.
  assertReleaseMatch({ manifest, release });
  // 3. Reject missing, extra, or edited resources.
  await verifyFiles({ directory, manifest });
  return manifest;
}

/** Read metadata without importing the application runtime. */
async function readManifest(directory) {
  const manifest = JSON.parse(await readFile(path.join(directory, 'manifest.json'), 'utf8'));
  if (manifest.source !== resourceSource || !['app', 'desktop'].includes(manifest.target))
    throw new Error('Invalid i18n resource source or target. Regenerate the snapshot.');
  versionParts(manifest.version);
  if (!manifest.files || typeof manifest.files !== 'object' || Array.isArray(manifest.files))
    throw new Error('Invalid i18n resource file manifest.');
  if (!Object.keys(manifest.files).length) throw new Error('Empty i18n resource manifest.');
  for (const [filename, hash] of Object.entries(manifest.files)) {
    if (!/^[a-zA-Z0-9_-]+(?:[/.][a-zA-Z0-9_-]+)*$/.test(filename) || filename === 'manifest.json')
      throw new Error(`Invalid i18n resource path: ${filename}`);
    if (typeof hash !== 'string' || !/^[a-f0-9]{64}$/.test(hash))
      throw new Error(`Invalid i18n resource hash: ${filename}`);
  }
  return manifest;
}

/** Compare a consumer artifact with the coordinated Website release. */
function assertReleaseMatch({ manifest, release }) {
  if (!release) return;
  if (release.source !== resourceSource || release.version !== manifest.version ||
      !isDeepStrictEqual(release.targets?.[manifest.target], manifest.files))
    throw new Error('I18n snapshot does not match the selected Website release. Regenerate it.');
}

/** Check all files, including this verifier, normalizing checkout newlines only. */
async function verifyFiles({ directory, manifest }) {
  const actual = (await listFiles(directory)).filter(filename => filename !== 'manifest.json').sort();
  const expected = Object.keys(manifest.files).sort();
  if (!isDeepStrictEqual(actual, expected)) throw new Error('I18n snapshot has missing or unexpected files. Regenerate it.');
  for (const filename of expected) {
    const content = await readFile(path.join(directory, filename), 'utf8');
    if (contentHash(content) !== manifest.files[filename])
      throw new Error(`I18n resource content changed: ${filename}. Regenerate the snapshot.`);
  }
}

/** List portable regular files; generated artifacts never need symlinks. */
async function listFiles(directory, prefix = '') {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) files.push(...await listFiles(path.join(directory, entry.name), relative));
    else if (entry.isFile()) files.push(relative);
    else throw new Error(`Unsupported i18n resource entry: ${relative}`);
  }
  return files;
}

/** Hash resource text consistently on all checkout platforms. */
function contentHash(content) {
  return createHash('sha256').update(content.replaceAll('\r\n', '\n')).digest('hex');
}

/** Resource releases use numeric major.minor.patch versions, without prerelease labels. */
function versionParts(version) {
  if (typeof version !== 'string' || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version))
    throw new Error('I18n resource version must be major.minor.patch.');
  return version.split('.').map(value => BigInt(value));
}

/** Reject downgrade or same-version content changes against a known earlier release. */
function assertVersionAdvance({ current, previous }) {
  const next = versionParts(current.version);
  if (!previous) return;
  const old = versionParts(previous.version);
  const difference = next.map((value, index) => value - old[index]).find(value => value !== 0n) ?? 0n;
  if (difference < 0n || (difference === 0n && !isDeepStrictEqual(current, previous)))
    throw new Error('Changed i18n resources require a higher release version.');
}

/** Offline by default; never infer a sibling checkout. */
async function main() {
  const [option, releasePath, ...extra] = process.argv.slice(2);
  if ((option && (option !== '--release' || !releasePath)) || extra.length)
    throw new Error('Usage: node verify-resources.cjs [--release <Website release-lock.json>]');
  const release = releasePath ? JSON.parse(await readFile(releasePath, 'utf8')) : undefined;
  const manifest = await verifySnapshot({ directory: __dirname, release });
  console.log(`Verified ${Object.keys(manifest.files).length} ${manifest.target} i18n files (${manifest.version})`);
}

module.exports = { verifySnapshot, contentHash, assertVersionAdvance, resourceSource };
if (require.main === module) main().catch(error => { console.error(error.message); process.exitCode = 1; });
