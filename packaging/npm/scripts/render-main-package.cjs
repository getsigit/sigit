#!/usr/bin/env node

const fs = require("fs");
const path = require("path");

const [outputPath, version, templateName = "package-main.json.tmpl"] =
  process.argv.slice(2);

if (!outputPath || !version) {
  throw new Error(
    "Usage: render-main-package.cjs <output-path> <version> [template-name]",
  );
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

// Read and interpolate the requested template: package-main.json.tmpl for
// @getsigit/sigit, package-compat.json.tmpl for the @smbcloud/sigit copy.
const template = fs.readFileSync(path.join(npmRoot, templateName), "utf-8");

fs.writeFileSync(path.resolve(outputPath), interpolate(template, vars));
