import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const debug = args.includes("--debug");
const targetIndex = args.indexOf("--target");
const rustc = spawnSync("rustc", ["-vV"], { encoding: "utf8" });
if (rustc.error || rustc.status !== 0) throw new Error("Cannot determine Rust host target");
const target = targetIndex >= 0 ? args[targetIndex + 1] : rustc.stdout.match(/^host: (\S+)$/m)?.[1];
if (!target || !/^[a-zA-Z0-9_-]+$/.test(target)) throw new Error("Invalid Rust target");
const cargoArgs = ["build", "--locked", "--target", target, "-p", "nebula-client", "-p", "nebula-agent"];
if (!debug) cargoArgs.push("--release");
const built = spawnSync("cargo", cargoArgs, { cwd: root, stdio: "inherit" });
if (built.error || built.status !== 0) process.exit(built.status ?? 1);
const suffix = target.includes("windows") ? ".exe" : "";
const profile = debug ? "debug" : "release";
const targetDir = process.env.CARGO_TARGET_DIR ? resolve(root, process.env.CARGO_TARGET_DIR) : join(root, "target");
const destination = join(root, "crates", "nebula-desktop", "binaries");
mkdirSync(destination, { recursive: true });
for (const name of ["nebula-client", "nebula-agent"]) {
  copyFileSync(join(targetDir, target, profile, `${name}${suffix}`), join(destination, `${name}-${target}${suffix}`));
}
