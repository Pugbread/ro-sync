import crypto from "node:crypto";
import fs from "node:fs";

const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

function decodeBase64(value, label) {
  const encoded = String(value || "").trim();
  if (!encoded || !/^[A-Za-z0-9+/]+={0,2}$/.test(encoded)) {
    throw new Error(`${label} is not canonical base64`);
  }
  const decoded = Buffer.from(encoded, "base64");
  const canonical = decoded.toString("base64").replace(/=+$/, "");
  if (canonical !== encoded.replace(/=+$/, "")) {
    throw new Error(`${label} is not canonical base64`);
  }
  return decoded;
}

function decodeEnvelope(value, label) {
  const decoded = decodeBase64(value, label).toString("utf8");
  if (decoded.includes("\uFFFD")) throw new Error(`${label} is not UTF-8`);
  return decoded.replace(/\r\n/g, "\n").replace(/\n+$/, "");
}

export function parseTauriPublicKey(value) {
  const lines = decodeEnvelope(value, "Tauri updater public key").split("\n");
  if (lines.length !== 2 || !lines[0].startsWith("untrusted comment:")) {
    throw new Error("Tauri updater public key has an invalid minisign envelope");
  }
  const packet = decodeBase64(lines[1], "minisign public-key packet");
  if (packet.length !== 42 || packet.subarray(0, 2).toString("ascii") !== "Ed") {
    throw new Error("Tauri updater public key must contain one Ed25519 key");
  }
  const keyId = packet.subarray(2, 10);
  const rawKey = packet.subarray(10);
  const key = crypto.createPublicKey({
    key: Buffer.concat([ED25519_SPKI_PREFIX, rawKey]),
    format: "der",
    type: "spki",
  });
  return { key, keyId, rawKey };
}

export function updaterPublicKeyFingerprint(value) {
  const { rawKey } = parseTauriPublicKey(value);
  return crypto.createHash("sha256").update(rawKey).digest("hex");
}

export function verifyPinnedUpdaterKey(pin, publicKey) {
  if (!pin || pin.schemaVersion !== 1) {
    throw new Error("desktop/updater-key.pin.json has an unsupported schema");
  }
  if (pin.algorithm !== "sha256-ed25519-public-key") {
    throw new Error("desktop/updater-key.pin.json has an unsupported fingerprint algorithm");
  }
  if (pin.state !== "configured" || !/^[a-f0-9]{64}$/.test(pin.publicKeySha256 || "")) {
    throw new Error(
      "Updater trust is not bootstrapped. Generate the production Tauri updater key once, "
      + "run `node scripts/check-updater-key-pin.mjs fingerprint <public-key-file>`, "
      + "commit that SHA-256 in desktop/updater-key.pin.json, and configure the matching "
      + "ROSYNC_UPDATER_PUBLIC_KEY repository variable before creating a release tag.",
    );
  }
  if (!String(publicKey || "").trim()) {
    throw new Error("ROSYNC_UPDATER_PUBLIC_KEY is required and must match the checked-in pin");
  }
  const actual = updaterPublicKeyFingerprint(publicKey);
  if (!crypto.timingSafeEqual(Buffer.from(actual, "hex"), Buffer.from(pin.publicKeySha256, "hex"))) {
    throw new Error(
      `Updater public-key fingerprint ${actual} does not match the reviewed pin ${pin.publicKeySha256}; refusing silent key rotation`,
    );
  }
  return actual;
}

export function readUpdaterKeyPin(path) {
  return JSON.parse(fs.readFileSync(path, "utf8"));
}

export function parseTauriSignature(value) {
  const envelope = String(value || "").trim();
  const lines = decodeEnvelope(envelope, "Tauri updater signature").split("\n");
  if (
    lines.length !== 4
    || !lines[0].startsWith("untrusted comment:")
    || !lines[2].startsWith("trusted comment: ")
  ) {
    throw new Error("Tauri updater signature has an invalid minisign envelope");
  }
  const packet = decodeBase64(lines[1], "minisign signature packet");
  const globalSignature = decodeBase64(lines[3], "minisign trusted-comment signature");
  if (packet.length !== 74 || globalSignature.length !== 64) {
    throw new Error("Tauri updater signature has an invalid packet length");
  }
  const algorithm = packet.subarray(0, 2).toString("ascii");
  if (algorithm !== "Ed" && algorithm !== "ED") {
    throw new Error(`unsupported minisign signature algorithm: ${algorithm}`);
  }
  return {
    envelope,
    algorithm,
    keyId: packet.subarray(2, 10),
    signature: packet.subarray(10),
    trustedComment: lines[2].slice("trusted comment: ".length),
    globalSignature,
  };
}

export function verifyTauriArtifactSignature({ artifactPath, signature, publicKey }) {
  const parsedKey = parseTauriPublicKey(publicKey);
  const parsedSignature = parseTauriSignature(signature);
  if (!crypto.timingSafeEqual(parsedKey.keyId, parsedSignature.keyId)) {
    throw new Error("updater signature key ID does not match the pinned public key");
  }
  const artifact = fs.readFileSync(artifactPath);
  const signed = parsedSignature.algorithm === "ED"
    ? crypto.createHash("blake2b512").update(artifact).digest()
    : artifact;
  if (!crypto.verify(null, signed, parsedKey.key, parsedSignature.signature)) {
    throw new Error(`invalid updater signature for ${artifactPath}`);
  }
  const globalMessage = Buffer.concat([
    parsedSignature.signature,
    Buffer.from(parsedSignature.trustedComment, "utf8"),
  ]);
  if (!crypto.verify(null, globalMessage, parsedKey.key, parsedSignature.globalSignature)) {
    throw new Error(`invalid trusted-comment signature for ${artifactPath}`);
  }
  return parsedSignature.envelope;
}
