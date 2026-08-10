/**
 * The exact slice of the node standard library the source-scraping tests use.
 *
 * The HUD is a browser bundle: its tsconfig lists no `node` types and nothing in
 * `src/` outside `src/test/` may touch the filesystem. `pixel-free-emits.test.ts`
 * has to read `daemon/src/**` — a gate that asserts a property of the DAEMON'S
 * SOURCE has no other way to see it — so rather than pulling `@types/node` (and a
 * lockfile bump) into a bundle that must never import node builtins, this
 * declares the seven functions actually used.
 *
 * `node:process`'s `stdout` is here for one reason: vitest 4's default reporter
 * swallows `console.log` from a passing test, so the measured list the gate
 * exists to publish has to go to the real stream to reach a terminal. A MODULE
 * import, never a global `process` declaration — a global would hand the browser
 * bundle a node symbol the type checker currently refuses it.
 *
 * Deliberately narrow: anything beyond these seven fails to compile, which keeps
 * the browser bundle's "no node builtins" property enforced by the type checker
 * instead of by a convention nobody rechecks. `vitest run` is the reality check
 * on the signatures.
 */
declare module "node:fs" {
  export function readFileSync(path: string | URL, encoding: "utf8"): string;
  export function readdirSync(path: string): string[];
  export function statSync(path: string): { isDirectory(): boolean };
}

declare module "node:path" {
  export function join(...parts: string[]): string;
  export function relative(from: string, to: string): string;
}

declare module "node:url" {
  export function fileURLToPath(url: string | URL): string;
}

declare module "node:process" {
  export const stdout: { write(chunk: string): boolean };
}
