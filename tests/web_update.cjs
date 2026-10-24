// Run with: node --test tests/web_update.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const page = fs.readFileSync('src/page.html', 'utf8');
const script = page.split('<script>')[1].split('  var box = document.getElementById("new-session");')[0] + '\n})();';

function dashboard(responses) {
  const elements = {};
  for (const id of ['su-btn', 'su-check', 'su-status']) {
    elements[id] = { hidden: id === 'su-btn', disabled: false, textContent: '',
      addEventListener(event, fn) { this[event] = fn; } };
  }
  const events = {}, calls = [], intervals = [];
  const document = { hidden: false, getElementById: id => elements[id],
    addEventListener: (event, fn) => { events[event] = fn; } };
  vm.runInNewContext(script, {
    document, window: { confirm: () => true, addEventListener: (event, fn) => { events[event] = fn; } },
    fetch: async (url, options) => {
      calls.push({ url, options });
      const result = responses.shift();
      assert.ok(result, 'unexpected request');
      if (result instanceof Error) throw result;
      const [status, body] = result;
      return { status, ok: status >= 200 && status < 300, json: async () => body, text: async () => String(body) };
    },
    setInterval: (fn, ms) => { assert.equal(ms, 60000); intervals.push(fn); },
    setTimeout: () => {},
  });
  return { elements, calls, events, document, poll: () => intervals[0]() };
}
const settle = () => new Promise(resolve => setImmediate(resolve));

test('manual check remains available after a negative or failed check and discovers a release', async () => {
  assert.match(page, /<div id="self-update">/);
  assert.match(page, /<button id="su-check" type="button">Check for updates/);
  const ui = dashboard([[200, { available: false }], [502, { error: 'offline' }],
    [200, { available: true, to: 'v99.0.0' }], [200, { updated: false }]]);
  await settle();
  assert.equal(ui.elements['su-check'].disabled, false);
  assert.equal(ui.elements['su-btn'].hidden, true);
  ui.elements['su-check'].click();
  await settle();
  assert.match(ui.elements['su-status'].textContent, /offline/);
  assert.equal(ui.elements['su-check'].disabled, false);
  ui.elements['su-check'].click();
  await settle();
  assert.equal(ui.calls[2].url, '/api/update/check');
  assert.equal(ui.calls[2].options.method, 'POST');
  assert.equal(ui.calls[2].options.headers['X-Omp-Deck-Action'], 'check-update');
  assert.equal(ui.elements['su-btn'].hidden, false);
  assert.equal(ui.elements['su-check'].disabled, false);
  ui.elements['su-btn'].click();
  await settle();
  assert.equal(ui.calls[3].url, '/api/self-update');
  assert.equal(ui.elements['su-btn'].hidden, true);
  assert.equal(ui.elements['su-check'].disabled, false);
});

test('visible polling and return to tab discover releases; disabled policy blocks actions', async () => {
  const ui = dashboard([[200, { available: false }], [200, { available: true, to: 'v99.0.0' }],
    [200, { available: false, disabled: true }]]);
  await settle();
  ui.document.hidden = true;
  ui.poll();
  assert.equal(ui.calls.length, 1);
  ui.document.hidden = false;
  ui.events.visibilitychange();
  await settle();
  assert.equal(ui.elements['su-btn'].hidden, false);
  ui.events.focus();
  await settle();
  assert.equal(ui.elements['su-check'].disabled, true);
  assert.equal(ui.elements['su-btn'].hidden, true);
  ui.poll();
  assert.equal(ui.calls.length, 3);
});

test('network failure remains retryable and a later poll recovers', async () => {
  const ui = dashboard([new Error('offline'), [200, { available: true, to: 'v99.0.0', error: 'offline' }],
    [200, { available: true, to: 'v99.0.0' }]]);
  await settle();
  assert.equal(ui.elements['su-check'].disabled, false);
  ui.poll();
  await settle();
  assert.equal(ui.elements['su-btn'].hidden, false);
  assert.match(ui.elements['su-status'].textContent, /offline/);
  ui.poll();
  await settle();
  assert.equal(ui.elements['su-status'].textContent, 'new release available');
});
