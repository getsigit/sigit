#!/usr/bin/env node

const fs = require("fs");
const path = require("path");

const [outputPath, version] = process.argv.slice(2);

if (!outputPath || !version) {
  throw new Error("Usage: render-main-package.cjs <output-path> <version>");
}

const npmRoot = path.resolve(__dirname, "..");

const vars = {
  release_version: version,
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

// Read and interpolate package-main.json.tmpl
const template = fs.readFileSync(
  path.join(npmRoot, "package-main.json.tmpl"),
  "utf-8",
);

fs.writeFileSync(path.resolve(outputPath), interpolate(template, vars));
