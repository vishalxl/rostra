// Run with `node --test crates/rostra-web-ui/tests/shoutbox-keyboard.js`.
// Exercise the shipped Alpine components with inert events and no network access.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(
  process.env.ROSTRA_APP_JS ?? new URL("../assets/app.js", import.meta.url),
  "utf8",
);

function loadComponents() {
  const components = new Map();
  let initialize;
  const document = {
    addEventListener(name, callback) {
      if (name === "alpine:init") initialize = callback;
    },
  };
  const window = {
    addEventListener() {},
  };
  const Alpine = {
    data(name, factory) {
      components.set(name, factory);
    },
  };

  vm.runInNewContext(source, {
    Alpine,
    document,
    history: { state: null },
    window,
    navigator: {},
    setTimeout,
    clearTimeout,
    URLSearchParams,
    console,
  });
  initialize();

  return {
    composer: components.get("shoutboxComposer")(),
    autocomplete: components.get("textAutocomplete")(),
  };
}

function keyEvent({
  key = "Enter",
  shiftKey = false,
  repeat = false,
  isComposing = false,
  keyCode = 13,
} = {}) {
  return {
    key,
    shiftKey,
    repeat,
    isComposing,
    keyCode,
    defaultPrevented: false,
    preventDefault() {
      this.defaultPrevented = true;
    },
  };
}

function harness() {
  const { composer, autocomplete } = loadComponents();
  let submissions = 0;
  const form = {
    requestSubmit() {
      submissions++;
    },
  };

  function keydown(event) {
    composer.handleSendKeydown(event, autocomplete.showDropdown);
    if (
      !(
        event.key === "Enter" &&
        (event.shiftKey || event.isComposing || event.keyCode === 229)
      )
    ) {
      autocomplete.handleKeydown.call(autocomplete, event);
    }
  }

  function keyup(event) {
    composer.handleSendKeyup(event, form);
  }

  return {
    composer,
    autocomplete,
    keydown,
    keyup,
    submissions: () => submissions,
  };
}

test("plain Enter prevents editing and submits once on release", () => {
  const app = harness();
  const initial = keyEvent();
  app.keydown(initial);
  assert.equal(initial.defaultPrevented, true);
  assert.equal(app.submissions(), 0);

  for (let count = 0; count < 3; count++) {
    const repeat = keyEvent({ repeat: true });
    app.keydown(repeat);
    assert.equal(repeat.defaultPrevented, true);
  }
  assert.equal(app.submissions(), 0);

  app.keyup(keyEvent());
  assert.equal(app.submissions(), 1);
  app.keyup(keyEvent());
  assert.equal(app.submissions(), 1);
});

test("shifted Enter remains an editing gesture across modifier release", () => {
  const app = harness();
  app.autocomplete.showDropdown = true;
  app.autocomplete.results = [{ type: "emoji", emoji: "x" }];
  app.autocomplete.selectResult = () =>
    assert.fail("shifted Enter selected autocomplete");

  const initial = keyEvent({ shiftKey: true });
  app.keydown(initial);
  assert.equal(initial.defaultPrevented, false);

  app.keyup(keyEvent());
  assert.equal(app.submissions(), 0);
});

test("autocomplete selection cannot become a send after closing the dropdown", () => {
  const app = harness();
  let selections = 0;
  app.autocomplete.showDropdown = true;
  app.autocomplete.results = [{ type: "emoji", emoji: "x" }];
  app.autocomplete.selectResult = () => {
    selections++;
    app.autocomplete.showDropdown = false;
  };

  const initial = keyEvent();
  app.keydown(initial);
  assert.equal(initial.defaultPrevented, true);
  assert.equal(selections, 1);

  for (let count = 0; count < 3; count++) {
    app.keydown(keyEvent({ repeat: true }));
  }
  app.keyup(keyEvent());
  assert.equal(selections, 1);
  assert.equal(app.submissions(), 0);

  app.keydown(keyEvent());
  app.keyup(keyEvent());
  assert.equal(app.submissions(), 1);
});

test("an open empty autocomplete does not send", () => {
  const app = harness();
  app.autocomplete.showDropdown = true;
  app.autocomplete.results = [];

  const initial = keyEvent();
  app.keydown(initial);
  assert.equal(initial.defaultPrevented, true);
  app.keyup(keyEvent());
  assert.equal(app.submissions(), 0);
});

test("composition and legacy composition Enter never arm a send", () => {
  for (const initial of [
    keyEvent({ isComposing: true }),
    keyEvent({ keyCode: 229 }),
  ]) {
    const app = harness();
    app.autocomplete.showDropdown = true;
    app.autocomplete.results = [{ type: "emoji", emoji: "x" }];
    app.autocomplete.selectResult = () =>
      assert.fail("composition Enter selected autocomplete");
    app.keydown(initial);
    assert.equal(initial.defaultPrevented, false);
    app.keyup(keyEvent());
    assert.equal(app.submissions(), 0);
  }

  for (const release of [
    keyEvent({ isComposing: true }),
    keyEvent({ keyCode: 229 }),
  ]) {
    const app = harness();
    app.keydown(keyEvent());
    app.keyup(release);
    assert.equal(app.submissions(), 0);
  }
});

test("non-Shift modifiers retain send behavior and blur clears eligibility", () => {
  for (const modifier of ["ctrlKey", "altKey", "metaKey"]) {
    const app = harness();
    const initial = keyEvent();
    initial[modifier] = true;
    app.keydown(initial);
    app.keyup(keyEvent());
    assert.equal(app.submissions(), 1, modifier);
  }

  const app = harness();
  app.keydown(keyEvent());
  app.composer.resetSendEligibility();
  app.keyup(keyEvent());
  assert.equal(app.submissions(), 0);
});
