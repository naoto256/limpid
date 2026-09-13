import assert from "node:assert/strict";
import { spawn } from "node:child_process";

async function within(promise, timeoutMs, error) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(error), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

export function spawnRecipe(command, args, options, timeoutMs = 10000) {
  const child = spawn(command, args, {
    ...options,
    shell: false,
    detached: false,
  });
  let processError;
  child.on("error", (error) => {
    processError ??= error;
  });
  // close also covers failed spawn and waits for this child's stdio to close.
  const ended = new Promise((resolve) =>
    child.once("close", (code, signal) => resolve([code, signal])),
  );
  const running = () =>
    child.pid !== undefined &&
    child.exitCode === null &&
    child.signalCode === null;
  let stopping;
  const stopOnce = async () => {
    try {
      if (running()) child.kill("SIGINT");
      const result = await within(
        ended,
        timeoutMs,
        Error(`Normal-stop deadline: PID ${child.pid}`),
      );
      if (processError) throw processError;
      assert.deepEqual(result, [0, null]);
    } catch (failure) {
      // Reap only our spawn handle. Forced cleanup never turns a failed normal stop into PASS.
      try {
        if (running()) child.kill("SIGKILL");
        await within(
          ended,
          5000,
          Error(`Owned child exit not confirmed: PID ${child.pid}`),
        );
      } catch (cleanupError) {
        throw new AggregateError(
          [failure, cleanupError],
          "Normal stop and owned-child cleanup failed",
          { cause: failure },
        );
      }
      throw failure;
    }
  };
  const stop = () => (stopping ??= stopOnce());
  return { child, ended, stop };
}

export async function withRecipeCleanup(run) {
  const cleanups = [];
  const errors = [];
  let result;
  try {
    result = await run((cleanup) => cleanups.push(cleanup));
  } catch (error) {
    errors.push(error);
  }
  for (const cleanup of cleanups.reverse()) {
    try {
      await cleanup();
    } catch (error) {
      if (!errors.includes(error)) errors.push(error);
    }
  }
  if (errors.length === 1) throw errors[0];
  if (errors.length > 1)
    throw new AggregateError(errors, "Recipe failed; cleanup also failed", {
      cause: errors[0],
    });
  return result;
}
