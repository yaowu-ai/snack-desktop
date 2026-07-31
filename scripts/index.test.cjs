const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const path = require("node:path");
const test = require("node:test");

test("accepts an explicit local frontend environment", () => {
  const result = spawnSync(
    process.execPath,
    [path.join(__dirname, "index.cjs"), "dev", "local", "--help"],
    {
      encoding: "utf8",
      env: { ...process.env, SNACK_ENV: "qa" },
    },
  );

  assert.equal(result.status, 0, result.stderr);
  assert.doesNotMatch(result.stderr, /Unknown dev environment/);
});
