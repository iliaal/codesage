import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { resolveContracts } from './resolve.mjs';

if (!process.env.TYPESCRIPT_PATH) throw new Error('Set TYPESCRIPT_PATH to the trusted TypeScript 5.9.2 typescript.js');
const compiler = fs.realpathSync(process.env.TYPESCRIPT_PATH);

function repo(t, files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'receiver-context-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  for (const [name, content] of Object.entries(files)) {
    const file = path.join(root, name);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, content);
  }
  execFileSync('git', ['init', '-q', root]);
  execFileSync('git', ['-C', root, 'add', '--', ...Object.keys(files)]);
  execFileSync('git', ['-C', root, '-c', 'user.name=Receiver Test', '-c', 'user.email=receiver@example.invalid',
    'commit', '-qm', 'fixture']);
  const head = execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  return { root, head };
}

function resolve(fixture, files, extra = {}) {
  return resolveContracts({ project: fixture.root, head: fixture.head, typescript: compiler, files, ...extra });
}

function named(result, name) {
  const matches = result.calls.filter((call) => call.name === name);
  assert.equal(matches.length, 1, `expected one ${name} call, got ${matches.length}`);
  return matches[0];
}

test('typed receiver chain follows an aliased barrel import to its implementation', (t) => {
  const fixture = repo(t, {
    'core.ts': 'export class Tab { goto(url: string) { return url; } }\nexport class Portal { tab = new Tab(); }\n',
    'barrel.ts': "export { Portal as PrimaryPortal } from './core';\n",
    'use.ts': "import { PrimaryPortal as Client } from './barrel';\nconst client = new Client();\nclient.tab.goto('home');\n",
  });
  const result = resolve(fixture, ['use.ts']);
  const call = named(result, 'goto');
  assert.equal(result.head, fixture.head);
  assert.equal(result.compiler.version, '5.9.2');
  assert.equal(call.status, 'candidate');
  assert.deepEqual(call.candidate, { path: 'core.ts', start: 1, end: 1, name: 'Tab.goto' });
  assert.deepEqual(call.receiver_chain.map(({ expression }) => expression), ['client', 'client.tab']);
  assert.equal(call.line, 3);
  assert.equal(result.diagnostics.count, 0);
});

test('same-named methods resolve according to the receiver type and lexical shadow', (t) => {
  const fixture = repo(t, {
    'use.ts': `class Target { goto() { return 'target'; } }
class Other { goto() { return 'other'; } }
const target = new Target();
target.goto();
function run(target: Other) { target.goto(); }
run(new Other());
`,
  });
  const result = resolve(fixture, ['use.ts']);
  const calls = result.calls.filter((call) => call.name === 'goto');
  assert.equal(calls.length, 2);
  assert.deepEqual(calls.map((call) => [call.line, call.status, call.candidate?.name, call.candidate?.start]), [
    [4, 'candidate', 'Target.goto', 1],
    [5, 'candidate', 'Other.goto', 2],
  ]);
});

test('an any-typed shadow does not inherit the outer binding target', (t) => {
  const fixture = repo(t, {
    'use.ts': `class Target { goto() {} }
const target = new Target();
function run(target: any) { target.goto(); }
target.goto();
`,
  });
  const result = resolve(fixture, ['use.ts']);
  const calls = result.calls.filter((call) => call.name === 'goto');
  assert.equal(calls.length, 2);
  assert.equal(calls[0].status, 'unresolved');
  assert.equal(calls[0].reason, 'unresolved-or-ambiguous-receiver');
  assert.equal(calls[0].candidate, undefined);
  assert.equal(calls[1].candidate?.name, 'Target.goto');
});

test('a union of distinct receiver implementations is ambiguous', (t) => {
  const fixture = repo(t, {
    'use.ts': `class A { go() {} }
class B { go() {} }
declare const either: A | B;
either.go();
`,
  });
  const call = named(resolve(fixture, ['use.ts']), 'go');
  assert.equal(call.status, 'unresolved');
  assert.equal(call.reason, 'unresolved-or-ambiguous-receiver');
  assert.equal(call.candidate, undefined);
});

test('a missing import and an any receiver yield no candidate', (t) => {
  const fixture = repo(t, {
    'use.ts': `import { missing } from './absent';
declare const unknownThing: any;
missing.go();
unknownThing.go();
`,
  });
  const result = resolve(fixture, ['use.ts']);
  const calls = result.calls.filter((call) => call.name === 'go');
  assert.equal(calls.length, 2);
  assert.ok(calls.every((call) => call.status === 'unresolved' && call.candidate === undefined));
  assert.equal(calls[1].reason, 'unresolved-or-ambiguous-receiver');
  assert.ok(result.diagnostics.codes[2307] >= 1, 'missing module must remain visible as a diagnostic');
});

test('an interface signature without a body is not an implementation', (t) => {
  const fixture = repo(t, {
    'use.ts': 'interface Contract { go(): void; }\ndeclare const contract: Contract;\ncontract.go();\n',
  });
  const call = named(resolve(fixture, ['use.ts']), 'go');
  assert.equal(call.status, 'unresolved');
  assert.equal(call.reason, 'no-implementation-declaration');
  assert.equal(call.candidate, undefined);
});

test('a computed method key does not claim a static target', (t) => {
  const fixture = repo(t, {
    'use.ts': 'class Target { go() {} }\nconst target = new Target();\ntarget["go"]();\n',
  });
  const call = named(resolve(fixture, ['use.ts']), 'target["go"]');
  assert.equal(call.status, 'unresolved');
  assert.equal(call.reason, 'unsupported-call-expression');
  assert.equal(call.candidate, undefined);
});

test('call locations use one-based LF lines and zero-based UTF-8 byte columns', (t) => {
  const fixture = repo(t, {
    'use.ts': 'class Target { go() {} }\nconst café = new Target();\n\n  café.go();\n',
  });
  const call = named(resolve(fixture, ['use.ts']), 'go');
  assert.equal(call.status, 'candidate');
  assert.equal(call.line, 4);
  assert.equal(call.col, Buffer.byteLength('  café.'));
});

test('a typed generic callback context resolves its destructured receiver', (t) => {
  const fixture = repo(t, {
    'framework.ts': `export class Page { goto(url: string) { return url; } }
export type Fixtures = { page: Page };
export function test<T extends Fixtures>(name: string, callback: (fixtures: T) => void) { void name; void callback; }
`,
    'use.ts': `import { test } from './framework';
test('visit', ({ page }) => { page.goto('/home'); });
`,
  });
  const result = resolve(fixture, ['use.ts']);
  const call = named(result, 'goto');
  assert.equal(call.status, 'candidate');
  assert.deepEqual(call.candidate, { path: 'framework.ts', start: 1, end: 1, name: 'Page.goto' });
  assert.equal(result.diagnostics.count, 0);
});

test('an external declaration package can type a project implementation through the dependency root', (t) => {
  const fixture = repo(t, {
    'use.ts': `import type { PageLike } from '@fixture/page';
class Page implements PageLike { goto(url: string) { return url; } }
const page = new Page();
page.goto('/home');
`,
  });
  const dependencyRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'receiver-deps-'));
  t.after(() => fs.rmSync(dependencyRoot, { recursive: true, force: true }));
  const packageDir = path.join(dependencyRoot, 'node_modules', '@fixture', 'page');
  fs.mkdirSync(packageDir, { recursive: true });
  fs.writeFileSync(path.join(packageDir, 'package.json'), JSON.stringify({ name: '@fixture/page', types: 'index.d.ts' }));
  fs.writeFileSync(path.join(packageDir, 'index.d.ts'), 'export interface PageLike { goto(url: string): string; }\n');
  const result = resolve(fixture, ['use.ts'], { dependencyRoot });
  assert.equal(named(result, 'goto').candidate?.name, 'Page.goto');
  assert.equal(result.diagnostics.count, 0);
  assert.ok(result.dependency_fallbacks.some((entry) => entry.startsWith('@fixture/page -> ')));
  assert.ok(result.reads.some((read) => read.role === 'dependency' && read.path.endsWith('index.d.ts')));
  assert.ok(result.reads.some((read) => read.role === 'compiler' && read.path.startsWith(path.dirname(compiler))));
});

test('modified working-tree source is refused against the pinned HEAD', (t) => {
  const fixture = repo(t, { 'use.ts': 'class Target { go() {} }\nnew Target().go();\n' });
  fs.writeFileSync(path.join(fixture.root, 'use.ts'), 'class Target { go() {} }\nconst target = new Target();\ntarget.go();\n');
  assert.throws(() => resolve(fixture, ['use.ts']), /Source differs from Git HEAD/);
});

test('a wrong pinned HEAD is refused before resolution', (t) => {
  const fixture = repo(t, { 'use.ts': 'class Target { go() {} }\n' });
  assert.throws(() => resolve(fixture, ['use.ts'], { head: '0'.repeat(40) }),
    /Expected full Git HEAD does not match project/);
});

test('compiler and dependency roots inside the project are refused', (t) => {
  const fixture = repo(t, {
    'use.ts': 'class Target { go() {} }\n',
    'vendor/typescript.js': 'module.exports = {};\n',
  });
  assert.throws(() => resolve(fixture, ['use.ts'], { typescript: path.join(fixture.root, 'vendor', 'typescript.js') }),
    /trusted compiler.*outside the reviewed project/);
  assert.throws(() => resolve(fixture, ['use.ts'], { dependencyRoot: path.join(fixture.root, 'vendor') }),
    /trusted compiler.*outside the reviewed project/);
});
