import test from "node:test";
import assert from "node:assert/strict";
import { copyCode } from "../src/copy-code.js";

function controls() {
  return {
    button: {
      disabled: false,
      innerHTML: "",
      setAttribute(name, value) {
        this[name] = value;
      },
    },
    status: { textContent: "" },
  };
}
test("copy preserves literal text and whitespace rather than highlighted HTML", async () => {
  const { button, status } = controls();
  const text = '  egress = "<tag> & ${key}"\n';
  let copied;
  await copyCode(
    { textContent: text, innerHTML: "not the payload" },
    button,
    status,
    {
      async writeText(value) {
        copied = value;
      },
    },
  );
  assert.equal(copied, text);
  assert.equal(status.textContent, "Copied");
  assert.equal(button["aria-label"], "Copied");
  assert.equal(button.disabled, false);
});
test("clipboard rejection or absence offers manual copying without claiming success", async () => {
  for (const clipboard of [
    undefined,
    {
      async writeText() {
        throw new Error("denied");
      },
    },
  ]) {
    const { button, status } = controls();
    await copyCode({ textContent: "code" }, button, status, clipboard);
    assert.match(status.textContent, /copy manually/);
    assert.equal(button.disabled, false);
  }
});
