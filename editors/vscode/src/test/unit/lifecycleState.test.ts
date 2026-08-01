import * as assert from "node:assert";
import { LspClientState } from "../../contracts/lifecycle";
import { resolveLifecycleState } from "../../lifecycleState";

suite("lifecycleState/resolveLifecycleState", () => {
  test("absent when no client and stopped", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: false,
        hasClient: false,
        clientState: LspClientState.Stopped,
      }),
      "absent"
    );
  });

  test("starting when client or state is starting", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: false,
        hasClient: false,
        clientState: LspClientState.Starting,
      }),
      "starting"
    );
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: false,
        hasClient: true,
        clientState: LspClientState.Starting,
      }),
      "starting"
    );
  });

  test("failed when start failed without client", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: false,
        hasClient: false,
        clientState: LspClientState.StartFailed,
      }),
      "failed"
    );
  });

  test("running when client is active", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: false,
        hasClient: true,
        clientState: LspClientState.Running,
      }),
      "running"
    );
  });

  test("disconnected when connection lost flag is set", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: true,
        hasClient: false,
        clientState: LspClientState.Stopped,
      }),
      "disconnected"
    );
  });

  test("disconnected when client exists but state is stopped", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: false,
        hasClient: true,
        clientState: LspClientState.Stopped,
      }),
      "disconnected"
    );
  });

  test("connectionLost takes precedence over running client", () => {
    assert.strictEqual(
      resolveLifecycleState({
        connectionLost: true,
        hasClient: true,
        clientState: LspClientState.Running,
      }),
      "disconnected"
    );
  });
});
