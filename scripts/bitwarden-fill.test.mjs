// node scripts/bitwarden-fill.test.mjs
import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";
import vm from "node:vm";

const rust = fs.readFileSync(new URL("../src-tauri/src/bitwarden.rs", import.meta.url), "utf8");
const script = rust.match(/const FILL_SCRIPT: &str = r#"([\s\S]*?)"#;/)[1];

class Input {
  constructor(type, autocomplete = "") {
    Object.assign(this, { type, autocomplete, disabled: false, readOnly: false, hidden: false,
      name: "", id: "", placeholder: "", events: [], focused: false, stored: "" });
  }
  get value() { return this.stored; }
  set value(value) { this.stored = value; }
  getClientRects() { return this.hidden ? [] : [{}]; }
  focus() { this.focused = true; }
  dispatchEvent(event) { this.events.push(event.type); }
}

function fill(inputs, actual = "https://example.org", expected = "https://example.org") {
  const context = vm.createContext({
    location: { origin: actual },
    document: { querySelectorAll: () => inputs },
    getComputedStyle: () => ({ visibility: "visible" }),
    HTMLInputElement: Input, Event,
  });
  vm.runInContext(`(${script})`, context)("fixture-user", "fixture-password", expected);
}

test("fills visible login fields through native setters and input/change events", () => {
  const user = new Input("text", "username"), pass = new Input("password");
  fill([user, pass]);
  assert.equal(user.value, "fixture-user"); assert.equal(pass.value, "fixture-password");
  assert.deepEqual(pass.events, ["input", "change"]); assert.ok(pass.focused);
});

test("origin is checked inside the injected script, including navigation races", () => {
  const user = new Input("text", "username"), pass = new Input("password");
  fill([user, pass], "https://other.example.org");
  assert.equal(user.value, ""); assert.equal(pass.value, ""); assert.equal(pass.events.length, 0);
});

test("ignores hidden and readonly fields and supports username-first forms", () => {
  const hidden = new Input("password"); hidden.hidden = true;
  const readonly = new Input("text", "username"); readonly.readOnly = true;
  const user = new Input("email", "username");
  fill([readonly, user, hidden]);
  assert.equal(user.value, "fixture-user"); assert.ok(user.focused);
  assert.equal(hidden.value, ""); assert.equal(readonly.value, "");
});
