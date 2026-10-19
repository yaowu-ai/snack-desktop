const { createHash } = require('node:crypto');
const { lstat, readFile, readdir } = require('node:fs/promises');
const path = require('node:path');
const { isDeepStrictEqual } = require('node:util');

const resourceSource = 'h-snack-website/src/lib/i18n';

/** Validate a pinned offline snapshot, optionally against current source resources. */
async function verifySnapshot({ directory, expected }) {
  const manifest = await readManifest(directory);
  await verifyFiles({ directory, manifest });
  assertSourceMatch({ manifest, expected });
  return manifest;
}

/** Read metadata without importing the application runtime. */
async function readManifest(directory) {
  const manifest = JSON.parse(await readFile(path.join(directory, 'manifest.json'), 'utf8'));
  if (manifest?.schemaVersion !== 1 || manifest.source !== resourceSource || !['app', 'desktop'].includes(manifest.target))
    throw new Error('Invalid i18n resource schema, source or target. Regenerate the snapshot.');
  validateFileHashes(manifest.files);
  if (manifest.fingerprint !== createManifest(manifest).fingerprint)
    throw new Error('Invalid i18n resource fingerprint. Regenerate the snapshot.');
  return manifest;
}

/** Restrict manifest ownership to portable payload paths and SHA256 hashes. */
function validateFileHashes(files) {
  if (!files || typeof files !== 'object' || Array.isArray(files))
    throw new Error('Invalid i18n resource file manifest.');
  if (!Object.keys(files).length) throw new Error('Empty i18n resource manifest.');
  for (const [filename, hash] of Object.entries(files)) {
    if (!/^[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*(?:\/[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*)*$/.test(filename) || filename === 'manifest.json')
      throw new Error(`Invalid i18n resource path: ${filename}`);
    if (typeof hash !== 'string' || !/^[a-f0-9]{64}$/.test(hash))
      throw new Error(`Invalid i18n resource hash: ${filename}`);
  }
  if (!Object.hasOwn(files, 'verify-resources.cjs')) throw new Error('Invalid i18n manifest: missing verifier.');
}

/** Derive a stable content identity without a manual version or Git baseline. */
function createManifest({ target, files }) {
  const metadata = { schemaVersion: 1, source: resourceSource, target };
  const entries = Object.keys(files).sort().map(filename => [filename, files[filename]]);
  const fingerprint = contentHash(JSON.stringify({ ...metadata, files: entries }));
  return { ...metadata, fingerprint, files: Object.fromEntries(entries) };
}

/** Compare only when a caller explicitly requests current-source freshness. */
function assertSourceMatch({ manifest, expected }) {
  if (expected && !isDeepStrictEqual(manifest, expected))
    throw new Error('I18n snapshot does not match current Website resources. Run i18n:sync.');
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
  if (!(await lstat(directory)).isDirectory()) throw new Error(`Unsupported i18n resource directory: ${directory}`);
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

/** Offline verification never infers a sibling checkout or the latest Website state. */
async function main() {
  if (process.argv.length !== 2) throw new Error('Usage: node verify-resources.cjs');
  const manifest = await verifySnapshot({ directory: __dirname });
  console.log(`Verified ${Object.keys(manifest.files).length} ${manifest.target} i18n files (${manifest.fingerprint})`);
}

module.exports = { verifySnapshot, contentHash, createManifest, validateFileHashes, listFiles };
if (require.main === module) main().catch(error => { console.error(error.message); process.exitCode = 1; });
