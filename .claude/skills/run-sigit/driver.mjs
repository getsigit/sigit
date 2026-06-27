#!/usr/bin/env node
// ACP driver for the `sigit` binary.
//
// `sigit` runs as an Agent Client Protocol server when stdin is NOT a TTY
// (newline-delimited JSON-RPC 2.0 over stdio — the same surface Zed / VS Code
// drive). This script spawns the binary in that mode, runs a scripted handshake,
// prints every request/response/notification, and exits non-zero on failure.
//
// It deliberately avoids triggering on-device inference: `initialize`,
// `session/new`, and slash commands like `/whoami` and `/status` answer without
// loading a multi-GB GGUF model, so the driver works on a clean machine with no
// model cached and no network.
//
// Usage:
//   node driver.mjs [path-to-binary]      # default: target/debug/sigit
//   SIGIT_BIN=target/release/sigit node driver.mjs
//
// Exit code 0 = every step got a well-formed JSON-RPC result.

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { existsSync } from "node:fs";

const bin = process.argv[2] || process.env.SIGIT_BIN || "target/debug/sigit";
if (!existsSync(bin)) {
  console.error(`binary not found: ${bin} — run \`cargo build\` first`);
  process.exit(2);
}

// Force ACP mode regardless of how the driver itself was launched: pipe stdin so
// the child's stdin is not a TTY.
const child = spawn(bin, [], { stdio: ["pipe", "pipe", "pipe"] });

// Surface the agent's own logs (it writes them to stderr in ACP mode).
createInterface({ input: child.stderr }).on("line", (l) =>
  console.error(`[sigit] ${l}`),
);

const pending = new Map(); // id -> {resolve, method}
const notifications = [];
let nextId = 1;
let failed = false;

createInterface({ input: child.stdout }).on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    console.error(`<-- (non-JSON) ${line}`);
    return;
  }
  if (msg.id !== undefined && (msg.result !== undefined || msg.error)) {
    const waiter = pending.get(msg.id);
    console.log(`<-- response #${msg.id} (${waiter?.method ?? "?"})`);
    console.log(JSON.stringify(msg.result ?? msg.error, null, 2));
    if (msg.error) failed = true;
    waiter?.resolve(msg);
    pending.delete(msg.id);
  } else if (msg.method) {
    // A notification or a server->client request. We only observe these.
    notifications.push(msg);
    const update = msg.params?.update;
    let detail = "";
    if (update?.sessionUpdate === "agent_message_chunk") {
      // The streamed assistant text — one chunk per AgentMessageChunk. With the
      // streaming backend a real prompt produces many of these.
      detail = ` ${JSON.stringify(update.content?.text ?? update.content)}`;
    } else if (update?.sessionUpdate) {
      detail = ` (${update.sessionUpdate})`;
    }
    console.log(`<-- notify ${msg.method}${detail}`);
  }
});

function send(method, params) {
  const id = nextId++;
  const req = { jsonrpc: "2.0", id, method, params };
  console.log(`--> request #${id} ${method}`);
  child.stdin.write(JSON.stringify(req) + "\n");
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, method });
    setTimeout(() => {
      if (pending.has(id)) {
        pending.delete(id);
        reject(new Error(`timeout waiting for ${method} (#${id})`));
      }
    }, 20_000);
  });
}

async function main() {
  // 1. Handshake.
  await send("initialize", {
    protocolVersion: 1,
    clientCapabilities: {},
  });

  // 2. Open a session rooted at the repo. `cwd` must be absolute.
  const sessionRes = await send("session/new", {
    cwd: process.cwd(),
    mcpServers: [],
  });
  const sessionId = sessionRes.result?.sessionId;
  if (!sessionId) throw new Error("session/new returned no sessionId");

  // 3. Drive a no-inference slash command through the prompt surface. `/whoami`
  //    reports the signed-in account; it never touches the model.
  await send("session/prompt", {
    sessionId,
    prompt: [{ type: "text", text: "/whoami" }],
  });

  console.log("\nOK — ACP handshake, session, and /whoami round-tripped.");
}

main()
  .catch((err) => {
    console.error(`FAILED: ${err.message}`);
    failed = true;
  })
  .finally(() => {
    child.kill("SIGTERM");
    setTimeout(() => process.exit(failed ? 1 : 0), 150);
  });
