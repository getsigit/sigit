#!/usr/bin/env node

const fs = require("fs");
const path = require("path");

const [packageName, version, operatingSystem, architecture] =
  process.argv.slice(2);

if (!packageName || !version || !operatingSystem || !architecture) {
  throw new Error(
    "Usage: render-platform-package.cjs <package-name> <version> <os> <arch>",
  );
}

const npmRoot = path.resolve(__dirname, "..");
const packageDirectory = path.resolve(npmRoot, packageName);

fs.mkdirSync(packageDirectory, { recursive: true });

// Variable map for template interpolation
const vars = {
  node_pkg: packageName,
  node_version: version,
  node_os: operatingSystem,
  node_arch: architecture,
};

/**
 * Replace every `${key}` in the template with the corresponding value from vars.
 */
function interpolate(template, variables) {
  return template.replace(/\$\{(\w+)\}/g, (match, key) => {
    if (key in variables) return variables[key];
    return match;
  });
}

// Read and interpolate package.json.tmpl
const packageTemplate = fs.readFileSync(
  path.join(npmRoot, "package.json.tmpl"),
  "utf-8",
);
fs.writeFileSync(
  path.join(packageDirectory, "package.json"),
  interpolate(packageTemplate, vars),
);

// Read and interpolate README.md.tmpl
const readmeTemplate = fs.readFileSync(
  path.join(npmRoot, "README.md.tmpl"),
  "utf-8",
);
fs.writeFileSync(
  path.join(packageDirectory, "README.md"),
  interpolate(readmeTemplate, vars),
);
