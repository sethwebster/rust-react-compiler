#!/usr/bin/env node
/**
 * dump_ts_hir.js — Run the React compiler on a fixture with debug logging
 * and dump the HIR state after each pass to /tmp/ts_hir/<fixture_name>/<pass>.txt
 *
 * Usage: node dump_ts_hir.js <path-to-fixture.js>
 */
'use strict';

const fs = require('fs');
const path = require('path');
const os = require('os');

const NODE_MODULES = '/home/claude-code/development/pepper/node_modules';
const BabelCore = require(path.join(NODE_MODULES, '@babel/core'));
const BabelParser = require(path.join(NODE_MODULES, '@babel/parser'));
const compiler = require(path.join(NODE_MODULES, 'babel-plugin-react-compiler/dist/index.js'));

const {printFunctionWithOutlined, printHIR, printReactiveFunctionWithOutlined, parseConfigPragmaForTests, Effect, ValueKind, ValueReason} = compiler;

let fixturePath = process.argv[2];
if (!fixturePath) {
  console.error('Usage: node dump_ts_hir.js <fixture.js|.ts|.tsx>');
  process.exit(1);
}

// Auto-detect extension if not found
if (!fs.existsSync(fixturePath)) {
  for (const ext of ['.js', '.ts', '.tsx', '.jsx']) {
    const candidate = fixturePath.replace(/\.(js|ts|tsx|jsx)$/, '') + ext;
    if (fs.existsSync(candidate)) { fixturePath = candidate; break; }
  }
}

const ext = path.extname(fixturePath);
const fixtureName = path.basename(fixturePath, ext);
const outDir = path.join('/tmp/ts_hir', fixtureName);
fs.mkdirSync(outDir, {recursive: true});

const source = fs.readFileSync(fixturePath, 'utf8');
const firstLine = source.split('\n')[0];

const passOutputs = {};

function debugIRLogger(value) {
  const {kind, name} = value;
  let text = '';
  try {
    if (kind === 'hir') {
      text = printFunctionWithOutlined(value.value);
    } else if (kind === 'reactive') {
      text = typeof printReactiveFunctionWithOutlined === 'function'
        ? printReactiveFunctionWithOutlined(value.value)
        : JSON.stringify(value.value, null, 2);
    } else if (kind === 'ast') {
      text = `[AST codegen output]\n`;
    } else if (kind === 'debug') {
      text = value.value;
    }
  } catch (e) {
    text = `[error printing: ${e.message}]`;
  }

  const safeName = name.replace(/[^a-zA-Z0-9_-]/g, '_');
  const count = (passOutputs[name] = (passOutputs[name] || 0) + 1);
  const filename = count > 1 ? `${safeName}_${count}.txt` : `${safeName}.txt`;
  const outPath = path.join(outDir, filename);
  fs.writeFileSync(outPath, `=== ${name} ===\n${text}\n`);
}

// Build logger
const logger = {
  logEvent: () => {},
  debugLogIRs: debugIRLogger,
};

// Parse config from fixture pragma
let config = {};
try {
  config = parseConfigPragmaForTests(firstLine, {compilationMode: 'all'});
} catch (e) {
  config = {compilationMode: 'all'};
}

const pluginOptions = {
  ...config,
  logger,
  enableReanimatedCheck: false,
};

// Parse source
const isFlow = source.includes('@flow');
const plugins = isFlow ? ['flow', 'jsx'] : ['typescript', 'jsx'];

let ast;
try {
  ast = BabelParser.parse(source, {
    sourceType: source.includes('@script') ? 'script' : 'module',
    plugins,
  });
} catch (e) {
  // Try hermes-parser if available
  try {
    const hermes = require(path.join(NODE_MODULES, 'hermes-parser'));
    ast = hermes.parse(source, {sourceType: 'module'});
  } catch (e2) {
    console.error('Parse failed:', e.message);
    process.exit(1);
  }
}

// Run compiler via babel transform
try {
  BabelCore.transformFromAstSync(ast, source, {
    filename: fixturePath,
    plugins: [[path.join(NODE_MODULES, 'babel-plugin-react-compiler/dist/index.js'), pluginOptions]],
    ast: false,
    code: true,
  });
} catch (e) {
  // Compilation errors are expected for error.* fixtures
  if (!path.basename(fixturePath).startsWith('error.')) {
    console.error('Compilation error:', e.message);
  }
}

const passes = fs.readdirSync(outDir).sort();
console.log(`Dumped ${passes.length} pass outputs to ${outDir}/`);
console.log('Passes:', passes.map(f => f.replace('.txt', '')).join(', '));
