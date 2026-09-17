#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const digest = (bytes) => crypto.createHash('sha256').update(bytes).digest('hex');
const within = (root, file) => file === root || file.startsWith(root + path.sep);
const MAX_FILES = 1024;
const MAX_BYTES = 64 * 1024 * 1024;

function git(root, ...args) {
  return execFileSync('git', ['-C', root, ...args], { maxBuffer: MAX_BYTES, encoding: 'utf8' }).trim();
}

function relative(root, file) {
  if (!within(root, file)) throw new Error(`Path outside project: ${file}`);
  return path.relative(root, file).split(path.sep).join('/');
}

function verifyGitReads(root, head, reads) {
  const projectReads = reads.filter((read) => read.role === 'project');
  const input = projectReads.map((read) => `${head}:${relative(root, read.path)}\n`).join('');
  const output = execFileSync('git', ['-C', root, 'cat-file', '--batch'], { input, maxBuffer: MAX_BYTES });
  let offset = 0;
  for (const read of projectReads) {
    const newline = output.indexOf(10, offset);
    const header = output.subarray(offset, newline).toString('utf8');
    const match = /^[0-9a-f]+ blob (\d+)$/.exec(header);
    if (!match) throw new Error(`Source not a tracked Git blob: ${read.path}`);
    const size = Number(match[1]);
    offset = newline + 1;
    if (digest(output.subarray(offset, offset + size)) !== read.sha256) {
      throw new Error(`Source differs from Git HEAD: ${read.path}`);
    }
    offset += size + 1;
  }
}

export function resolveContracts(args) {
  const root = fs.realpathSync(args.project);
  const head = git(root, 'rev-parse', 'HEAD');
  if (!/^[0-9a-f]{40,64}$/.test(args.head ?? '') || head !== args.head) {
    throw new Error('Expected full Git HEAD does not match project');
  }
  const compilerPath = fs.realpathSync(args.typescript);
  const compilerRoot = path.dirname(compilerPath);
  const dependencyRoot = args.dependencyRoot ? fs.realpathSync(args.dependencyRoot) : null;
  if (within(root, compilerPath) || (dependencyRoot && within(root, dependencyRoot))) {
    throw new Error('Use a trusted compiler and optional dependency directory outside the reviewed project');
  }
  const compilerHash = digest(fs.readFileSync(compilerPath));
  const ts = createRequire(import.meta.url)(compilerPath);
  if (ts.version !== '5.9.2') throw new Error(`Supported TypeScript version is 5.9.2; got ${ts.version}`);
  const reads = new Map();
  let bytes = 0;
  function permitted(file) {
    const absolute = path.resolve(file);
    if (![root, compilerRoot, dependencyRoot].filter(Boolean).some((dir) => within(dir, absolute))) return null;
    if (!fs.existsSync(absolute)) return null;
    const real = fs.realpathSync(absolute);
    if (![root, compilerRoot, dependencyRoot].filter(Boolean).some((dir) => within(dir, real))) return null;
    return real;
  }
  function readFile(file) {
    const real = permitted(file);
    if (!real || !fs.statSync(real).isFile()) return undefined;
    if (/[\r\n\0]/.test(real)) throw new Error('Unsupported control character in source path');
    const role = within(root, real) ? 'project' : within(compilerRoot, real) ? 'compiler' : 'dependency';
    if (role === 'dependency' && !/\.d\.(?:ts|mts|cts)$|\.json$/.test(real)) return undefined;
    if (!reads.has(real) && (reads.size >= MAX_FILES || fs.statSync(real).size > MAX_BYTES - bytes)) {
      throw new Error('Compiler input budget exceeded');
    }
    const raw = fs.readFileSync(real);
    if (!reads.has(real)) {
      bytes += raw.length;
      if (reads.size >= MAX_FILES || bytes > MAX_BYTES) throw new Error('Compiler input budget exceeded');
    }
    const text = raw.toString('utf8');
    if (!Buffer.from(text).equals(raw)) throw new Error(`Non-UTF-8 compiler input: ${real}`);
    if (/\r(?!\n)/.test(text)) throw new Error(`Bare CR source rows are unsupported: ${real}`);
    const hash = digest(raw);
    if (reads.has(real) && reads.get(real).sha256 !== hash) throw new Error(`Input changed during resolution: ${real}`);
    reads.set(real, { path: real, sha256: hash, role });
    return text;
  }
  const hostSystem = {
    useCaseSensitiveFileNames: true,
    readFile,
    fileExists: (file) => Boolean(permitted(file) && fs.statSync(permitted(file)).isFile()),
    directoryExists: (file) => Boolean(permitted(file) && fs.statSync(permitted(file)).isDirectory()),
    readDirectory: () => [],
    getCurrentDirectory: () => root,
    realpath: (file) => permitted(file) ?? file,
  };
  let options = { target: ts.ScriptTarget.ES2021, module: ts.ModuleKind.CommonJS,
    moduleResolution: ts.ModuleResolutionKind.Node10, strict: true, types: [] };
  let configPath = null;
  if (args.tsconfig) {
    configPath = fs.realpathSync(path.resolve(root, args.tsconfig));
    relative(root, configPath);
    const config = ts.readConfigFile(configPath, readFile);
    if (config.error) throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, '\n'));
    const parsed = ts.parseJsonConfigFileContent(config.config, hostSystem, path.dirname(configPath), {}, configPath);
    const errors = parsed.errors.filter((error) => error.code !== 18003);
    if (errors.length) throw new Error(errors.map((e) => ts.flattenDiagnosticMessageText(e.messageText, '\n')).join('\n'));
    options = parsed.options;
  }
  options = { ...options, noEmit: true, incremental: false, composite: false, skipLibCheck: true };
  const host = ts.createCompilerHost(options, true);
  Object.assign(host, hostSystem);
  host.useCaseSensitiveFileNames = () => true;
  host.writeFile = () => { throw new Error('Compiler emission is disabled'); };
  host.getSourceFile = (file, languageVersion) => {
    const text = readFile(file);
    return text === undefined ? undefined : ts.createSourceFile(file, text, languageVersion, true);
  };
  const fallbackFile = dependencyRoot && path.join(dependencyRoot, 'input.ts');
  const fallbacks = new Set();
  const moduleResolutions = [];
  host.resolveModuleNames = (names, containing) => names.map((name) => {
    const direct = ts.resolveModuleName(name, containing, options, host).resolvedModule;
    const fallback = !direct && fallbackFile && !name.startsWith('.') && !path.isAbsolute(name)
      ? ts.resolveModuleName(name, fallbackFile, options, host).resolvedModule : undefined;
    if (fallback) fallbacks.add(`${name} -> ${fallback.resolvedFileName}`);
    const resolved = direct ?? fallback;
    moduleResolutions.push({ from: containing, specifier: name, resolved: resolved?.resolvedFileName ?? null });
    return resolved;
  });
  host.resolveTypeReferenceDirectives = (names, containing) => names.map((entry) => {
    const name = typeof entry === 'string' ? entry : entry.fileName;
    return ts.resolveTypeReferenceDirective(name, containing, options, host).resolvedTypeReferenceDirective
      ?? (fallbackFile && ts.resolveTypeReferenceDirective(name, fallbackFile, options, host).resolvedTypeReferenceDirective);
  });
  const files = args.files.map((file) => {
    const real = fs.realpathSync(path.resolve(root, file));
    relative(root, real);
    if (!/\.(ts|tsx|mts|cts)$/.test(real) || real.includes('/node_modules/')) throw new Error(`Not project TypeScript: ${file}`);
    return real;
  });
  if (!files.length || new Set(files).size !== files.length) throw new Error('Provide distinct TypeScript source files');
  const program = ts.createProgram(files, options, host);
  const checker = program.getTypeChecker();
  const allDiagnostics = [...program.getOptionsDiagnostics(), ...program.getSyntacticDiagnostics(), ...program.getSemanticDiagnostics()];
  function location(node) {
    const source = node.getSourceFile();
    const absolute = path.resolve(source.fileName);
    const start = node.getStart(source);
    const before = source.text.slice(0, start);
    return {
      path: within(root, absolute) ? relative(root, absolute) : absolute,
      start: before.split('\n').length,
      end: source.text.slice(0, node.getEnd()).split('\n').length,
      col: Buffer.byteLength(before.slice(before.lastIndexOf('\n') + 1)),
    };
  }
  function declarations(node) {
    let symbol = checker.getSymbolAtLocation(node);
    if (symbol?.flags & ts.SymbolFlags.Alias) symbol = checker.getAliasedSymbol(symbol);
    return symbol?.declarations ?? [];
  }
  function declarationName(node) {
    const own = node.name?.getText() ?? '';
    const parent = node.parent;
    return parent && (ts.isClassDeclaration(parent) || ts.isClassExpression(parent)) && parent.name
      ? `${parent.name.text}.${own}` : own;
  }
  const calls = [];
  for (const file of files) {
    const source = program.getSourceFile(file);
    if (!source) throw new Error(`Compiler did not load ${file}`);
    function visit(node) {
      if (calls.length >= 10000) throw new Error('Call-site budget exceeded');
      if (ts.isCallExpression(node)) {
        const expression = node.expression;
        const token = ts.isPropertyAccessExpression(expression) ? expression.name : expression;
        const loc = location(token);
        const row = { path: relative(root, file), line: loc.start, col: loc.col,
          name: token.getText(), expression: expression.getText(), status: 'unresolved', receiver_chain: [] };
        let reason = null;
        if (!ts.isIdentifier(expression) && !ts.isPropertyAccessExpression(expression)) reason = 'unsupported-call-expression';
        let receiver = ts.isPropertyAccessExpression(expression) ? expression.expression : null;
        while (receiver && !reason) {
          if (!ts.isIdentifier(receiver) && !ts.isPropertyAccessExpression(receiver) && receiver.kind !== ts.SyntaxKind.ThisKeyword) {
            reason = 'unsupported-receiver-expression';
            break;
          }
          const type = checker.getTypeAtLocation(receiver);
          row.receiver_chain.unshift({ expression: receiver.getText(), type: checker.typeToString(type),
            declarations: declarations(receiver).map(location),
            type_declarations: (type.aliasSymbol ?? type.symbol)?.declarations?.map(location) ?? [] });
          if (type.flags & (ts.TypeFlags.Any | ts.TypeFlags.Unknown | ts.TypeFlags.TypeParameter | ts.TypeFlags.Union | ts.TypeFlags.Intersection | ts.TypeFlags.Never)) {
            reason = 'unresolved-or-ambiguous-receiver';
          }
          receiver = ts.isPropertyAccessExpression(receiver) ? receiver.expression : null;
        }
        if (!reason && allDiagnostics.some((error) => error.category === ts.DiagnosticCategory.Error
          && error.file === source && error.start < node.end && error.start + (error.length ?? 1) > node.pos)) {
          reason = 'call-site-diagnostic';
        }
        const targets = reason ? [] : declarations(token);
        const bodies = targets.filter((target) => (ts.isMethodDeclaration(target) || ts.isFunctionDeclaration(target)) && target.body);
        if (!reason && bodies.length !== 1) reason = bodies.length ? 'ambiguous-declaration' : 'no-implementation-declaration';
        if (!reason) {
          const target = bodies[0];
          const targetFile = path.resolve(target.getSourceFile().fileName);
          if (!within(root, targetFile) || targetFile.includes('/node_modules/')) reason = 'external-declaration';
          else {
            const targetLoc = location(target);
            row.status = 'candidate';
            row.candidate = { path: targetLoc.path, start: targetLoc.start, end: targetLoc.end, name: declarationName(target) };
          }
        }
        if (reason) row.reason = reason;
        calls.push(row);
      }
      ts.forEachChild(node, visit);
    }
    visit(source);
  }
  const readList = [...reads.values()].sort((a, b) => a.path.localeCompare(b.path));
  verifyGitReads(root, head, readList);
  for (const read of readList) {
    if (digest(fs.readFileSync(read.path)) !== read.sha256) throw new Error(`Input changed during resolution: ${read.path}`);
  }
  if (git(root, 'rev-parse', 'HEAD') !== head || digest(fs.readFileSync(compilerPath)) !== compilerHash) {
    throw new Error('HEAD or compiler changed during resolution');
  }
  const diagnosticCounts = {};
  for (const diagnostic of allDiagnostics) diagnosticCounts[diagnostic.code] = (diagnosticCounts[diagnostic.code] ?? 0) + 1;
  const dependencyPackages = readList.filter((read) => read.role === 'dependency' && path.basename(read.path) === 'package.json')
    .map((read) => {
      const metadata = JSON.parse(fs.readFileSync(read.path, 'utf8'));
      return { path: read.path, name: metadata.name, version: metadata.version, sha256: read.sha256 };
    });
  return { schema: 1, kind: 'typescript-contract-candidates', project: root, head,
    compiler: { path: compilerPath, version: ts.version, sha256: compilerHash },
    tsconfig: configPath, scope: 'explicit roots and imported source; no project-reference build or runtime dispatch proof',
    options, dependency_fallbacks: [...fallbacks].sort(), dependency_packages: dependencyPackages,
    module_resolutions: moduleResolutions, reads: readList, calls,
    diagnostics: { count: allDiagnostics.length, codes: diagnosticCounts,
      examples: allDiagnostics.slice(0, 20).map((d) => ({ code: d.code, file: d.file?.fileName,
        message: ts.flattenDiagnosticMessageText(d.messageText, '\n') })), truncated: allDiagnostics.length > 20 } };
}

function main(argv) {
  if (argv.includes('--help')) {
    console.log('Usage: node resolve.mjs --project ROOT --head SHA --typescript TRUSTED/typescript.js [--tsconfig PATH] [--dependency-root TRUSTED_NPM_PREFIX] --file PATH [--file PATH...] --output NEW.json');
    return;
  }
  const args = { files: [] };
  const names = { '--project': 'project', '--head': 'head', '--typescript': 'typescript',
    '--tsconfig': 'tsconfig', '--dependency-root': 'dependencyRoot', '--output': 'output' };
  for (let i = 0; i < argv.length; i += 2) {
    if (!argv[i + 1] || argv[i + 1].startsWith('--')) throw new Error(`Missing value: ${argv[i]}`);
    if (argv[i] === '--file') args.files.push(argv[i + 1]);
    else if (names[argv[i]] && !args[names[argv[i]]]) args[names[argv[i]]] = argv[i + 1];
    else throw new Error(`Unknown or repeated option: ${argv[i]}`);
  }
  for (const key of ['project', 'head', 'typescript', 'output']) if (!args[key]) throw new Error(`Missing --${key}`);
  if (fs.existsSync(args.output)) throw new Error(`Output already exists: ${args.output}`);
  const result = resolveContracts(args);
  fs.writeFileSync(args.output, JSON.stringify(result, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
  console.log(`Resolved ${result.calls.filter((call) => call.status === 'candidate').length}/${result.calls.length} declaration candidates; ${result.diagnostics.count} compiler diagnostics`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try { main(process.argv.slice(2)); }
  catch (error) { console.error(error.message); process.exitCode = 1; }
}
