#!/usr/bin/env node
// Long-running helper Bridge spawns while a game profile has button
// assignments for a Logitech mouse running from software (Host mode).
//
// stdin: one JSON line to start, e.g. {"brand": "Logitech"}. Closing stdin (or
//   SIGTERM/SIGINT) stops it: the spy is switched off and the mouse goes back
//   to the mode it was in, if this helper changed it.
// stdout: one JSON object per line.
//   {"event": "ready", "priorMode": "Onboard", "buttons": [1, 2, 3, 4, 5]}
//   {"event": "buttons", "mask": 16}   — the buttons held right now
//   {"event": "error", "message": "..."}
//   {"event": "stopped"}
//
// Exit codes: 0 stopped cleanly, 3 no driver for the brand, 1 anything else.
// See apply.mjs for why `window` is aliased before any driver is imported.
globalThis.window ??= globalThis;

import { createInterface } from "node:readline";

import { BRAND_DRIVERS, brandKey } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const EXIT_OK = 0;
const EXIT_FAILED = 1;
const EXIT_NO_DRIVER = 3;
const STEP_TIMEOUT_MS = 3000;

const emit = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Opens a candidate class on one device and reads its status, or rejects and closes it. */
async function probe(device, candidate) {
  const module = await import(candidate.module);
  const client = new module[candidate.exportName](device);
  try {
    await withTimeout(client.open(), STEP_TIMEOUT_MS, `${candidate.exportName}.open()`);
    const status = await withTimeout(client.readStatus(), STEP_TIMEOUT_MS, `${candidate.exportName}.readStatus()`);
    return { client, status };
  } catch (error) {
    await device.close().catch(() => undefined);
    throw error;
  }
}

async function findClient(entry) {
  const attempts = [];
  for (const vendorId of entry.vendorIds) {
    for (const device of candidateDevices(vendorId)) {
      for (const candidate of entry.classes) {
        try {
          const found = await probe(device, candidate);
          // The spy only exists on a client built from a protocol release that has it.
          if (typeof found.client.startButtonSpy !== "function") {
            await found.client.close().catch(() => undefined);
            attempts.push(`${candidate.exportName}: this protocol version has no button spy`);
            continue;
          }
          return found;
        } catch (error) {
          attempts.push(`${candidate.exportName} on ${device.productName || "unknown device"}: ${error.message}`);
        }
      }
    }
  }
  throw new Error(`no mouse answered. ${attempts.join("; ")}`);
}

async function main(input) {
  const entry = BRAND_DRIVERS[brandKey(typeof input.brand === "string" ? input.brand : "")];
  if (!entry) {
    emit({ event: "error", message: `no driver registered for brand "${input.brand}"` });
    process.exit(EXIT_NO_DRIVER);
  }

  const { client, status } = await findClient(entry);
  const priorMode = status?.deviceMode ?? "Unknown";
  let changedMode = false;
  let stopping = false;

  async function shutdown() {
    if (stopping) return;
    stopping = true;
    await withTimeout(client.stopButtonSpy(), STEP_TIMEOUT_MS, "stopButtonSpy").catch(() => undefined);
    if (changedMode) {
      await withTimeout(client.setOnboardMode(priorMode), STEP_TIMEOUT_MS, "setOnboardMode").catch(() => undefined);
    }
    await client.close().catch(() => undefined);
    emit({ event: "stopped" });
    process.exit(EXIT_OK);
  }

  process.stdin.on("end", shutdown);
  process.stdin.on("close", shutdown);
  process.on("SIGTERM", shutdown);
  process.on("SIGINT", shutdown);

  try {
    // The events only flow while the mouse runs from software; an unknown mode
    // is left alone and the spy is tried as it is.
    if (priorMode === "Onboard") {
      await withTimeout(client.setOnboardMode("Host"), STEP_TIMEOUT_MS, "setOnboardMode");
      changedMode = true;
    }
    client.onButtonSpy((mask) => emit({ event: "buttons", mask }));
    const buttons = await withTimeout(client.startButtonSpy(), STEP_TIMEOUT_MS, "startButtonSpy");
    emit({ event: "ready", priorMode, buttons });
  } catch (error) {
    emit({ event: "error", message: error.message });
    await shutdown();
    process.exit(EXIT_FAILED);
  }
}

const lines = createInterface({ input: process.stdin });
lines.once("line", (line) => {
  let input;
  try {
    input = JSON.parse(line);
  } catch {
    emit({ event: "error", message: "the first line on stdin must be JSON" });
    process.exit(EXIT_FAILED);
  }
  main(input).catch((error) => {
    emit({ event: "error", message: error.message });
    process.exit(EXIT_FAILED);
  });
});
