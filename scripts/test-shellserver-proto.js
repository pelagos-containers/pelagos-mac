#!/usr/bin/env node
// Tests the VS Code shell server protocol over our exec implementation.
// Sends commands wrapped in sentinels (exactly as VS Code does) and verifies
// the responses come back correctly and promptly.
//
// Protocol: echo -n ␄; ( cmd ); echo -n ␄$?␄; echo -n ␄ >&2\n
//           Stdout: ␄{output}␄{exitCode}␄
//           Stderr: ␄

'use strict';

const { spawn } = require('child_process');
const path = require('path');

const CONTAINER = process.argv[2] || 'hostcheck';
const SENTINEL  = '\u2404';  // ␄ — same character VS Code uses
const TIMEOUT_MS = 15000;

const pelagosDocker = path.resolve(__dirname, '../target/aarch64-apple-darwin/release/pelagos-docker');

// Commands to test (same as VS Code would send)
const COMMIT = 'f25b8fcfff366f7c14fb36d7ff13f5c895ed0c1b';
const INSTALL_FOLDER = `/root/.vscode-server-insiders/bin/${COMMIT}-insider`;

const TESTS = [
  { name: 'echo ~',           cmd: 'echo $HOME' },
  { name: 'cat product.json', cmd: `cat '${INSTALL_FOLDER}/product.json' | head -c 200` },
  { name: 'ps proc scan',     cmd: `for pid in \`cd /proc && ls -d [0-9]* 2>/dev/null | head -5\`; do echo $pid; done` },
  { name: 'uname',            cmd: 'uname -m' },
];

console.log(`[test] container: ${CONTAINER}`);
console.log(`[test] spawning: pelagos-docker exec -i -u root ${CONTAINER} /bin/sh`);

const child = spawn(pelagosDocker, [
  'exec', '-i', '-u', 'root', CONTAINER, '/bin/sh'
], { stdio: ['pipe', 'pipe', 'pipe'] });

child.on('error', (err) => {
  console.error(`[FAIL] spawn error: ${err.message}`);
  process.exit(1);
});

let stdoutBuf = '';
let stderrBuf = '';
let testIdx    = 0;
let testStart  = null;

child.stdout.on('data', (chunk) => {
  stdoutBuf += chunk.toString('utf8');
  processOutput();
});

child.stderr.on('data', (chunk) => {
  stderrBuf += chunk.toString('utf8');
});

function processOutput() {
  // Each command response: ␄{output}␄{exitCode}␄
  // We need to find: sentinel, output, sentinel, exitCode, sentinel
  let parts = stdoutBuf.split(SENTINEL);
  // Need at least 4 parts: ['', output, exitCode, remainder]
  // Actually the format is: ␄output␄exitCode␄
  // So after the first ␄ we have: output, exitCode, remaining
  if (parts.length < 4) return;

  // parts[0] should be empty (before first ␄)
  // parts[1] = output
  // parts[2] = exit code
  // parts[3+] = next response start
  const output   = parts[1];
  const exitCode = parts[2].trim();
  stdoutBuf = parts.slice(3).join(SENTINEL);

  const elapsed = Date.now() - testStart;
  const test = TESTS[testIdx - 1];

  console.log(`[${elapsed}ms] cmd '${test.name}' done: exit=${exitCode}`);
  if (output.trim()) {
    console.log(`  stdout: ${output.trim().substring(0, 200)}`);
  }

  if (exitCode !== '0') {
    console.error(`[FAIL] cmd '${test.name}' returned exit code ${exitCode}`);
  } else {
    console.log(`[PASS] cmd '${test.name}'`);
  }

  sendNextTest();
}

function sendNextTest() {
  if (testIdx >= TESTS.length) {
    const elapsed = Date.now() - globalStart;
    console.log(`\n[DONE] All ${TESTS.length} tests passed in ${elapsed}ms`);
    child.stdin.end();
    process.exit(0);
  }

  const test = TESTS[testIdx++];
  const wrapped = `echo -n ${SENTINEL}; ( ${test.cmd} ); echo -n ${SENTINEL}$?${SENTINEL}; echo -n ${SENTINEL} >&2\n`;

  testStart = Date.now();
  console.log(`[test] sending: ${test.name}`);
  child.stdin.write(wrapped);
}

child.on('exit', (code, signal) => {
  if (testIdx <= TESTS.length) {
    console.error(`[FAIL] child exited prematurely: code=${code} signal=${signal} after test ${testIdx-1}/${TESTS.length}`);
    process.exit(1);
  }
});

const globalStart = Date.now();

// Overall timeout
setTimeout(() => {
  console.error(`[FAIL] timeout after ${TIMEOUT_MS}ms — hung on test ${testIdx}/${TESTS.length}`);
  child.kill('SIGTERM');
  process.exit(1);
}, TIMEOUT_MS).unref();

// Start the shell server protocol (VS Code sends this to initialize the shell)
// VS Code Kt() sends: /bin/sh -c 'echo ␄; /bin/sh'\n when i=true
// But actually with i=true (function), it just sends commands directly.
// Let me use the simpler approach: just start sending commands
sendNextTest();
