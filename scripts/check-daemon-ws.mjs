import assert from "node:assert/strict";

globalThis.window = {
  addEventListener() {},
  parent: { postMessage() {} },
};

const sockets = [];
class FakeWebSocket {
  constructor(url) {
    this.url = url;
    this.readyState = 0;
    this.sent = [];
    sockets.push(this);
  }

  open() {
    this.readyState = 1;
    this.onopen?.({ type: "open" });
  }

  receive(data) {
    this.onmessage?.({ data: JSON.stringify(data) });
  }

  close() {
    if (this.readyState === 3) return;
    this.readyState = 3;
    this.onclose?.({ type: "close", code: 1000 });
  }

  send(data) {
    assert.equal(this.readyState, 1, "test socket must be open before send");
    this.sent.push(JSON.parse(data));
  }
}
globalThis.WebSocket = FakeWebSocket;

const { daemonWS } = await import(`../bridge.js?daemon-ws-test=${Date.now()}`);

let readyCount = 0;
const stream = daemonWS("http://127.0.0.1:7878", "/ws", {
  open: () => { readyCount++; },
});
assert.equal(sockets.length, 1);
sockets[0].open();
assert.deepEqual(
  sockets[0].sent.map((frame) => frame.type),
  ["hello", "ping"],
  "watch sockets must authenticate and request an acknowledgement",
);
assert.equal(readyCount, 0, "a raw WebSocket open is not an authenticated daemon connection");
sockets[0].receive({ type: "pong" });
assert.equal(readyCount, 1, "the first accepted daemon frame publishes ready exactly once");
sockets[0].receive({ type: "pong" });
assert.equal(readyCount, 1);
stream.close();

let rejectedReadyCount = 0;
const rejected = daemonWS("http://127.0.0.1:7879", "/ws", {
  open: () => { rejectedReadyCount++; },
});
sockets[1].open();
sockets[1].receive({ type: "shutdown", retryable: false, reason: "rejected" });
assert.equal(rejectedReadyCount, 0, "a shutdown rejection must never publish ready");
rejected.close();

console.log("daemon WebSocket authentication checks passed");
