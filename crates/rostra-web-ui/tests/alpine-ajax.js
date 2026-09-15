// Run with `node --test crates/rostra-web-ui/tests/alpine-ajax.js`.
// Exercise the shipped bundle with inert DOM targets and no network access.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const bundle = readFileSync(
  process.env.ROSTRA_ALPINE_AJAX_BUNDLE ??
    new URL("../assets/libs/alpine-ajax@0.12.6.js", import.meta.url),
  "utf8",
);

function harness(fetch) {
  const attributes = new Map();
  const events = [];
  let initialize;
  let ajax;
  let loadingTimer;
  let loading = false;
  const target = {
    isConnected: true,
    querySelectorAll: () => [],
    setAttribute: (key, value) => attributes.set(key, value),
    removeAttribute: (key) => attributes.delete(key),
  };
  const element = {
    getAttribute: () => null,
    closest: () => null,
    dispatchEvent(event) {
      events.push(event);
      if (event.type === "ajax:before") {
        loadingTimer = setTimeout(() => { loading = true; }, 150);
      }
      if (event.type === "ajax:after") {
        clearTimeout(loadingTimer);
        loading = false;
      }
      return true;
    },
  };
  const Alpine = {
    addInitSelector() {},
    directive() {},
    magic(name, callback) {
      if (name === "ajax") ajax = callback(element);
    },
  };
  const document = {
    baseURI: "http://localhost/messages",
    documentElement: target,
    querySelectorAll: () => [],
    createRange: () => ({
      createContextualFragment: () => ({ firstElementChild: { content: {} } }),
    }),
    addEventListener(name, callback) {
      if (name === "alpine:initializing") initialize = callback;
    },
  };
  vm.runInNewContext(bundle, {
    document,
    window: { Alpine, addEventListener() {} },
    fetch,
    URL,
    setTimeout,
    console,
    DOMException,
    CustomEvent: class {
      constructor(type, options) {
        this.type = type;
        this.detail = options.detail;
      }
    },
  });
  initialize();
  return {
    request: (method) => ajax("/messages/test", { method, targets: ["_none"] }),
    events,
    attributes,
    isLoading: () => loading,
  };
}

for (const method of ["GET", "POST"]) {
  for (const failure of ["fetch", "response-body"]) {
    test(`${method} ${failure} rejection releases state and permits an explicit retry`, async () => {
      let requests = 0;
      const app = harness(async () => {
        requests++;
        if (requests === 1) {
          await new Promise((resolve) => setTimeout(resolve, 180));
          if (failure === "fetch") throw new TypeError("offline");
          return { text: async () => { throw new TypeError("connection lost"); } };
        }
        return {
          ok: true,
          status: 200,
          redirected: false,
          url: "http://localhost/messages/test",
          text: async () => "",
        };
      });
      await assert.rejects(app.request(method), TypeError);
      assert.equal(requests, 1, "never replay an uncertain POST automatically");
      assert.equal(app.isLoading(), false);
      assert.equal(app.attributes.has("aria-busy"), false);
      const error = app.events.find((event) => event.type === "ajax:error");
      assert.equal(error.detail.status, 0, "shared notifications recognize transport errors");
      assert.equal(app.events.at(-1).type, "ajax:after");
      assert.equal(app.events.some((event) => event.type === "ajax:success"), false);

      await app.request(method);
      assert.equal(requests, 2, "the failed request must not stay in the GET cache");
      assert.equal(app.isLoading(), false);
      assert.equal(app.attributes.has("aria-busy"), false);
      assert.equal(app.events.some((event) => event.type === "ajax:success"), true);
    });
  }
}
