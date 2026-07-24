#!/usr/bin/env node
import { readFile } from "node:fs/promises";

function assert(condition, message) {
  if (!condition) {
    throw new Error(`Windows release policy check failed: ${message}`);
  }
}

function includes(source, text, message) {
  assert(source.includes(text), message);
}

function matches(source, pattern, message) {
  assert(pattern.test(source), message);
}

function count(source, text) {
  return source.split(text).length - 1;
}

function ordered(source, first, second, message) {
  const firstIndex = source.indexOf(first);
  const secondIndex = source.indexOf(second);
  assert(firstIndex >= 0 && secondIndex >= 0 && firstIndex < secondIndex, message);
}

const [ci, release, windowsBuild] = await Promise.all([
  readFile(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8"),
  readFile(new URL("../.github/workflows/release.yml", import.meta.url), "utf8"),
  readFile(new URL("../daemon/build.ps1", import.meta.url), "utf8"),
]);

includes(
  ci,
  "node scripts/check-windows-release-policy.mjs",
  "CI must run this policy check",
);
matches(
  ci,
  /Exercise Windows source build without optional tool downloads[\s\S]*?ROSYNC_SKIP_TOOL_DOWNLOAD:\s*"1"[\s\S]*?run:\s*\.\/daemon\/build\.ps1/,
  "Windows CI must exercise daemon/build.ps1 with optional downloads disabled",
);
matches(
  ci,
  /Build unsigned Windows MSI and NSIS installers[\s\S]*?--bundles msi,nsis --ci --no-sign/,
  "Windows CI must build unsigned MSI and NSIS bundles",
);
matches(
  ci,
  /Check unsigned Windows MSI and NSIS outputs[\s\S]*?Filter '\*\.msi'[\s\S]*?Filter '\*-setup\.exe'/,
  "Windows CI must require both MSI and NSIS outputs",
);

includes(
  windowsBuild,
  "$env:ROSYNC_SKIP_TOOL_DOWNLOAD -eq '1'",
  "daemon/build.ps1 must expose the no-download opt-out used by CI",
);
ordered(
  windowsBuild,
  "$skipToolDownload",
  "install-luau-compiler.mjs",
  "daemon/build.ps1 must evaluate the opt-out before attempting a tool download",
);

assert(
  count(release, "ROSYNC_TAGGED_RELEASE: ${{ github.event_name == 'push'") >= 2,
  "standalone and desktop jobs must derive the same tag-push signing gate",
);
assert(
  !release.includes('if [[ "$GITHUB_REF_TYPE"'),
  "release signing decisions must use ROSYNC_TAGGED_RELEASE, not raw GITHUB_REF_TYPE",
);
includes(
  release,
  "if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')",
  "only a pushed v-tag may publish a GitHub release",
);
includes(release, "workflow_dispatch:", "manual unsigned release builds must remain supported");

for (const secret of [
  "WINDOWS_CODESIGN_PFX_BASE64",
  "WINDOWS_CODESIGN_PFX_PASSWORD",
]) {
  assert(
    count(release, `secrets.${secret}`) === 2,
    `${secret} must be provided independently to standalone and desktop jobs`,
  );
}
assert(
  count(release, "Import-PfxCertificate") === 2,
  "standalone and desktop jobs must each import their own PFX",
);
assert(
  count(release, "Cert:\\CurrentUser\\My") >= 4,
  "PFX import and cleanup must use the CurrentUser My store in both jobs",
);
assert(
  count(release, "tagged Windows releases require repository secrets") === 2,
  "missing tagged-release secrets must fail clearly in both Windows jobs",
);

ordered(
  release,
  "- name: Sign and verify standalone Windows daemon",
  "- name: Stage artifact",
  "the standalone Windows daemon must be signed before hashing and bundling",
);
matches(
  release,
  /Sign and verify standalone Windows daemon[\s\S]*?sign `[\s\S]*?\/fd SHA256 `[\s\S]*?\/tr 'http:\/\/timestamp\.digicert\.com' `[\s\S]*?\/td SHA256 `[\s\S]*?verify \/pa \/all \/tw \$binary/,
  "standalone signing must use SHA-256, RFC3161 timestamping, and strict verification",
);
includes(
  release,
  'RAW_PATH="$dist/${{ matrix.out }}" ARCHIVE_PATH="$archive"',
  "standalone and bundle checksums must be generated together",
);
includes(
  release,
  "daemon/dist/${{ matrix.out }}.sha256",
  "standalone daemon checksums must be uploaded",
);
matches(
  release,
  /for daemon in "\$\{standalone_daemons\[@\]\}"; do[\s\S]*?sha256sum --check --strict -- "\$daemon\.sha256"/,
  "publication must verify every standalone daemon checksum",
);

matches(
  release,
  /if: runner\.os == 'Windows' && env\.ROSYNC_TAGGED_RELEASE == '1'/,
  "Windows signing steps must use the shared tag-push gate",
);
for (const configText of [
  "certificateThumbprint: process.env.WINDOWS_CODESIGN_THUMBPRINT",
  'digestAlgorithm: "sha256"',
  "timestampUrl: process.env.WINDOWS_TIMESTAMP_URL",
  "tsp: true",
]) {
  includes(release, configText, `tagged Tauri config must include ${configText}`);
}
matches(
  release,
  /if \[\[ "\$ROSYNC_TAGGED_RELEASE" != "1" \]\]; then[\s\S]*?build_args\+=\(--no-sign\)/,
  "manual builds must explicitly disable signing",
);
ordered(
  release,
  "- name: Stage desktop release assets",
  "- name: Verify final Windows installer Authenticode signatures",
  "the copied release installers must be verified after staging",
);
matches(
  release,
  /Verify final Windows installer Authenticode signatures[\s\S]*?desktop\/release-assets\/Ro-Sync-\$env:ROSYNC_DESKTOP_VERSION-windows-x64\.msi[\s\S]*?desktop\/release-assets\/Ro-Sync-\$env:ROSYNC_DESKTOP_VERSION-windows-x64-setup\.exe[\s\S]*?verify \/pa \/all \/tw \$installer/,
  "final copied MSI and NSIS artifacts must pass Authenticode verification",
);
assert(
  count(release, "Remove imported Windows Authenticode certificate") === 2,
  "both Windows jobs must clean their imported signing certificate",
);

console.log("Windows release policy checks passed");
