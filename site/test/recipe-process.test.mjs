import test from "node:test";
import assert from "node:assert/strict";
import { once } from "node:events";
import { spawnRecipe, withRecipeCleanup } from "./recipe-process.mjs";

async function fixture(t, ignore = false) {
  const owned = spawnRecipe(
    process.execPath,
    [
      "-e",
      `
    process.on('SIGINT', () => ${ignore ? "{}" : "process.exit(0)"});
    setInterval(() => {}, 1000);
    process.stdout.write('ready');
  `,
    ],
    { stdio: ["ignore", "pipe", "ignore"] },
    100,
  );
  // Test emergency cleanup only ever targets this test's own ChildProcess.
  t.after(async () => {
    if (owned.child.exitCode === null && owned.child.signalCode === null) {
      owned.child.kill("SIGKILL");
      await owned.ended;
    }
  });
  await once(owned.child.stdout, "data");
  return owned;
}

test("normal exit and repeated cleanup succeed", async (t) => {
  const owned = await fixture(t);
  await owned.stop();
  await owned.stop();
  assert.equal(owned.child.exitCode, 0);
});

test("timeout remains FAIL after owned child is reaped; cleanup is idempotent", async (t) => {
  const owned = await fixture(t, true);
  const signals = [];
  const kill = owned.child.kill.bind(owned.child);
  owned.child.kill = (signal) => {
    signals.push(signal);
    return kill(signal);
  };
  const first = owned.stop();
  const second = owned.stop();
  const settled = await Promise.allSettled([first, second]);
  assert.equal(settled[0].status, "rejected");
  assert.match(settled[0].reason.message, /Normal-stop deadline/);
  assert.equal(owned.child.signalCode, "SIGKILL");
  assert.equal(first, second);
  assert.equal(settled[0].reason, settled[1].reason);
  await assert.rejects(owned.stop(), (error) => error === settled[0].reason);
  assert.deepEqual(signals, ["SIGINT", "SIGKILL"]);
});

test("setup failure preserves original error and stops acquired receiver", async (t) => {
  const owned = await fixture(t);
  const failure = Error("setup failed");
  await assert.rejects(
    withRecipeCleanup(async (defer) => {
      defer(owned.stop);
      throw failure;
    }),
    (error) => error === failure,
  );
  assert.equal(owned.child.exitCode, 0);
});

test("all cleanup runs and preserves original plus cleanup errors", async (t) => {
  const owned = await fixture(t, true);
  const failure = Error("setup failed");
  const cleanupFailure = Error("sink failed");
  let lastRan = false;
  await assert.rejects(
    withRecipeCleanup(async (defer) => {
      defer(() => {
        lastRan = true;
      });
      defer(owned.stop);
      defer(() => {
        throw cleanupFailure;
      });
      throw failure;
    }),
    (error) => {
      assert.equal(error.cause, failure);
      assert.equal(error.errors[0], failure);
      assert.ok(error.errors.includes(cleanupFailure));
      assert.match(error.errors[2].message, /Normal-stop deadline/);
      return true;
    },
  );
  assert.ok(lastRan);
  assert.equal(owned.child.signalCode, "SIGKILL");
});

test("already exited child is checked without another signal", async () => {
  for (const code of [0, 7]) {
    const owned = spawnRecipe(
      process.execPath,
      ["-e", `process.exit(${code})`],
      { stdio: "ignore" },
    );
    await owned.ended;
    owned.child.kill = () => {
      throw Error("unexpected signal");
    };
    if (code === 0) await owned.stop();
    else await assert.rejects(owned.stop(), { code: "ERR_ASSERTION" });
  }
});

test("failed spawn reports original ENOENT without unhandled rejection", async () => {
  const owned = spawnRecipe("/nonexistent-limpid-recipe-fixture", [], {
    stdio: "ignore",
  });
  await assert.rejects(owned.stop(), { code: "ENOENT" });
  assert.equal(owned.child.pid, undefined);
});

test("failure before acquisition does not invent cleanup", async () => {
  const failure = Error("pre-spawn failure");
  await assert.rejects(
    withRecipeCleanup(async () => {
      throw failure;
    }),
    (error) => error === failure,
  );
});

test("forced recovery leaves a separately owned child untouched", async (t) => {
  const stuck = await fixture(t, true);
  const other = await fixture(t);
  await assert.rejects(stuck.stop(), /Normal-stop deadline/);
  assert.equal(stuck.child.signalCode, "SIGKILL");
  assert.equal(other.child.exitCode, null);
  assert.equal(other.child.signalCode, null);
  await other.stop();
});

test("normal signal exception is preserved after owned child recovery", async (t) => {
  const owned = await fixture(t);
  const failure = Error("normal signal failed");
  const kill = owned.child.kill.bind(owned.child);
  owned.child.kill = (signal) => {
    if (signal === "SIGINT") throw failure;
    return kill(signal);
  };
  await assert.rejects(owned.stop(), (error) => error === failure);
  assert.equal(owned.child.signalCode, "SIGKILL");
});
