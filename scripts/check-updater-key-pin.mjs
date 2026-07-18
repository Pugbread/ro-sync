#!/usr/bin/env node
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import {
  readUpdaterKeyPin,
  updaterPublicKeyFingerprint,
  verifyPinnedUpdaterKey,
} from "./updater-trust.mjs";

const [command = "verify", argument] = process.argv.slice(2);
const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

try {
  if (command === "fingerprint") {
    if (!argument) throw new Error("usage: check-updater-key-pin.mjs fingerprint <public-key-file>");
    process.stdout.write(`${updaterPublicKeyFingerprint(fs.readFileSync(argument, "utf8"))}\n`);
  } else if (command === "verify") {
    const pinPath = path.resolve(
      process.env.ROSYNC_UPDATER_KEY_PIN || path.join(repositoryRoot, "desktop/updater-key.pin.json"),
    );
    const publicKey = argument
      ? fs.readFileSync(argument, "utf8").trim()
      : process.env.ROSYNC_UPDATER_PUBLIC_KEY;
    const fingerprint = verifyPinnedUpdaterKey(readUpdaterKeyPin(pinPath), publicKey);
    process.stdout.write(`verified pinned updater public key ${fingerprint}\n`);
  } else {
    throw new Error(`unknown command: ${command}`);
  }
} catch (error) {
  console.error(`updater key check failed: ${error.message}`);
  process.exitCode = 1;
}
