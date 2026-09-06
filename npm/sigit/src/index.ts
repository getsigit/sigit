#!/usr/bin/env node

import { spawnSync } from "child_process";

/**
 * The same launcher ships under two names: `@getsigit/sigit`, and
 * `@smbcloud/sigit`, which is what the package was called before 1.5.5. Each
 * one carries its own set of platform packages, so which scope to look the
 * binary up in is whichever scope this copy was published under.
 */
function getScope() {
  const selfName = require("../package.json").name as string;
  const scope = selfName.split("/")[0];
  return scope.startsWith("@") ? scope : "@getsigit";
}

/**
 * Returns the executable path which is located inside node_modules
 * The naming convention is cli-OS-ARCH
 * If the platform is win32 or cygwin, executable will include a .exe extension.
 * @see https://nodejs.org/api/os.html#osarch
 * @see https://nodejs.org/api/os.html#osplatform
 * @example "x/xx/node_modules/cli-darwin-arm64"
 */
function getExePath() {
  const arch = process.arch;
  let os = process.platform as string;
  let extension = "";
  if (["win32", "cygwin"].includes(process.platform)) {
    os = "windows";
    extension = ".exe";
  }

  try {
    // Since the binary will be located inside node_modules, we can simply call require.resolve
    return require.resolve(
      `${getScope()}/sigit-${os}-${arch}/bin/sigit${extension}`,
    );
  } catch (e) {
    throw new Error(
      `Couldn't find application binary inside node_modules for ${os}-${arch}`,
    );
  }
}

/**
 * `@smbcloud/sigit` is kept publishing so that an install predating the rename
 * keeps updating instead of sitting on 1.5.2, but `@getsigit/sigit` is the
 * name the project actually goes by. Say so, once per run, on stderr — in ACP
 * mode stdout is the JSON-RPC pipe to the editor.
 */
function warnIfDeprecatedName() {
  if (getScope() !== "@smbcloud") return;
  if (process.env.SIGIT_SUPPRESS_SCOPE_NOTICE) return;

  process.stderr.write(
    "@smbcloud/sigit is the old name for @getsigit/sigit. Both names get the " +
      "same release for now; the new one is where siGit Code lives:\n" +
      "  npm uninstall -g @smbcloud/sigit && npm install -g @getsigit/sigit\n" +
      "Set SIGIT_SUPPRESS_SCOPE_NOTICE=1 to silence this.\n",
  );
}

/**
 * Runs the application with args using nodejs spawn
 */
function run() {
  warnIfDeprecatedName();
  const args = process.argv.slice(2);
  const processResult = spawnSync(getExePath(), args, { stdio: "inherit" });
  process.exit(processResult.status ?? 0);
}

run();
