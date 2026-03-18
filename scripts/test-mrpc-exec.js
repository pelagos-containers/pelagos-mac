#!/usr/bin/env node
// Tests the full VS Code muxrpc protocol for starting a remote server.
//
// From remoteContainersServer.js manifest:
//   vi = { exec:"async", stdin:"sink", stdout:"source", stderr:"source",
//           exit:"async", terminate:"async", ... }
//
// Flow:
//   1. Spawn exec -i /bin/sh, write launch command, wait for sentinel on stderr
//   2. Receive connected() and ready() from server (async, reqId=1,2)
//   3. Respond to both
//   4. Call exec({cmd,args,env}) — async — get back processId (integer)
//   5. Call stdout(processId) — source — receive stream of stdout chunks
//   6. Watch for "Extension host agent listening on PORT"
//
// Usage: node scripts/test-mrpc-exec.js [container]

'use strict';

const { spawn } = require('child_process');
const path = require('path');

const CONTAINER  = process.argv[2] || 'hostcheck';
const COMMIT     = 'f25b8fcfff366f7c14fb36d7ff13f5c895ed0c1b';
const SENTINEL   = '\u2404';
const TIMEOUT_MS = 20000;

const nodeExe    = `/root/.vscode-server-insiders/bin/${COMMIT}-insider/node`;
const serverMain = `/root/.vscode-server-insiders/bin/${COMMIT}-insider/out/server-main.js`;
const serverScript = `/root/.vscode-remote-containers/dist/vscode-remote-containers-server-0.450.0.js`;
const token = 'test-' + Date.now();

const pelagosDocker = path.resolve(__dirname, '../target/aarch64-apple-darwin/release/pelagos-docker');

const t0 = Date.now();
const ms = () => `[${Date.now()-t0}ms]`;

// ---------------------------------------------------------------------------
// packet-stream-codec wire format (from remoteContainersServer.js source):
//   byte 0:   flags = stream<<3 | end<<2 | type
//               type: 0=buffer(Fr), 1=string(_r), 2=JSON(Or)
//               end:  bit 2 (0x04)
//               stream: bit 3 (0x08)
//   bytes 1-4: body length (big-endian uint32)
//   bytes 5-8: request id  (big-endian int32, +ve=request, -ve=response)
//
// Combined flags:
//   0x02 = JSON non-stream non-end  → async REQUEST  (from either side)
//   0x06 = JSON end    non-stream   → async RESPONSE (from either side)
//   0x0A = JSON stream non-end      → source SUBSCRIPTION or stream DATA CHUNK
//   0x0E = JSON stream end          → stream END / abort
//   0x0A + body=null                → pull-stream DEMAND (sink asks source for next item)
// ---------------------------------------------------------------------------
const F_ASYNC_REQ  = 0x02;  // async request
const F_ASYNC_RESP = 0x06;  // async response (end, no stream)
const F_SRC_SUB    = 0x0A;  // source subscription / stream chunk / demand

function encode(flags, body, reqId) {
  const b = Buffer.isBuffer(body) ? body
           : typeof body === 'string' ? Buffer.from(body)
           : Buffer.from(JSON.stringify(body));
  const h = Buffer.alloc(9);
  h[0] = flags;
  h.writeInt32BE(b.length, 1);
  h.writeInt32BE(reqId, 5);
  return Buffer.concat([h, b]);
}

function decode(buf) {
  const frames = [];
  let off = 0;
  while (off + 9 <= buf.length) {
    const flags = buf[off];
    const len   = buf.readInt32BE(off + 1);
    const reqId = buf.readInt32BE(off + 5);
    if (off + 9 + len > buf.length) break;
    const body = buf.slice(off + 9, off + 9 + len);
    frames.push({ flags, reqId, body });
    off += 9 + len;
  }
  return { frames, rest: buf.slice(off) };
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
console.log(`${ms()} container: ${CONTAINER}`);
console.log(`${ms()} server:    ${serverScript}`);

const child = spawn(pelagosDocker, [
  'exec', '-i', '-u', 'root', CONTAINER, '/bin/sh'
], { stdio: ['pipe', 'pipe', 'pipe'] });

child.on('error', e => { console.error(`[FAIL] spawn: ${e.message}`); process.exit(1); });

let stderrBuf   = '';
let stdoutBuf   = Buffer.alloc(0);
let sentinelOK  = false;
let state       = 'wait_sentinel';   // wait_sentinel → wait_server_calls → wait_exec → wait_stdout → done
let processId   = null;
let stdoutReqId = null;              // our reqId for stdout() call
let execOutput  = '';
let serverPort  = null;
let myReqId     = 0;                 // counter for our outgoing requests

const ovTimer = setTimeout(() => {
  console.error(`[FAIL] timeout after ${TIMEOUT_MS}ms — state=${state} processId=${processId}`);
  console.error(`  execOutput: ${execOutput.substring(0, 400)}`);
  child.kill('SIGTERM');
  process.exit(1);
}, TIMEOUT_MS);

// Sentinel watch
child.stderr.on('data', chunk => {
  stderrBuf += chunk.toString('utf8');
  if (!sentinelOK && stderrBuf.includes(SENTINEL)) {
    sentinelOK = true;
    state = 'wait_server_calls';
    console.log(`${ms()} SENTINEL — muxrpc session started`);
    stderrBuf = '';
  }
  if (stderrBuf.trim()) {
    // Ignore non-sentinel stderr (might be our encoded responses echoed back)
    stderrBuf = '';
  }
});

// muxrpc frames on stdout
child.stdout.on('data', chunk => {
  stdoutBuf = Buffer.concat([stdoutBuf, chunk]);
  const { frames, rest } = decode(stdoutBuf);
  stdoutBuf = rest;
  for (const fr of frames) onFrame(fr);
});

child.on('exit', (code, sig) => {
  if (!serverPort) {
    console.error(`${ms()} [FAIL] exec session exited: code=${code} sig=${sig} state=${state}`);
    process.exit(1);
  }
});

function write(flags, body, reqId) {
  const buf = encode(flags, body, reqId);
  console.log(`${ms()} → reqId=${reqId} flags=0x${flags.toString(16)} len=${buf.length-9}`);
  child.stdin.write(buf);
}

function onFrame({ flags, reqId, body }) {
  const isStream = !!(flags & 0x08);  // bit 3
  const isEnd    = !!(flags & 0x04);  // bit 2
  const typeVal  = flags & 0x03;      // 0=buf, 1=str, 2=JSON
  // For our purposes: treat all as text
  const data     = body.toString('utf8');

  if (reqId > 0) {
    // Incoming request from server
    let parsed;
    try { parsed = JSON.parse(data); } catch(e) { parsed = data; }
    const name = parsed && parsed.name && parsed.name[0];
    console.log(`${ms()} ← server req reqId=${reqId} name=${name} flags=0x${flags.toString(16)} stream=${isStream} end=${isEnd}`);

    if (name === 'connected' || name === 'ready') {
      // Respond: reqId=-reqId, flags=F_ASYNC_RESP, body=[false,null]
      write(F_ASYNC_RESP, [false, null], -reqId);
      if (name === 'ready') {
        console.log(`${ms()} ready() — calling exec()`);
        setTimeout(callExec, 10);
      }
    } else {
      write(F_ASYNC_RESP, [false, null], -reqId);
    }
  } else if (reqId < 0) {
    // Response to one of our requests (reqId = -(ourReqId))
    const ourReq = -reqId;
    console.log(`${ms()} ← response ourReq=${ourReq} flags=0x${flags.toString(16)} len=${body.length} data=${data.toString().substring(0,100)}`);

    if (state === 'wait_exec' && ourReq === 1) {
      // exec() response — body should be [false, processId]
      let parsed;
      try { parsed = JSON.parse(data); } catch(e) { parsed = null; }
      if (Array.isArray(parsed) && parsed[0] === true) {
        console.error(`[FAIL] exec() error: ${JSON.stringify(parsed[1])}`);
        process.exit(1);
      }
      const pid = Array.isArray(parsed) ? parsed[1] : parsed;
      processId = pid;
      console.log(`${ms()} exec() → processId=${processId}`);
      state = 'wait_stdout';
      callStdout(processId);
    } else if (state === 'wait_stdout' && ourReq === 2) {
      // stdout(processId) response chunk or end
      if (isEnd) {
        // End of stdout stream (process exited?)
        console.log(`${ms()} stdout() stream ended: ${JSON.stringify(data)}`);
        if (!serverPort) {
          console.error(`[FAIL] stdout stream ended without seeing listening port`);
          console.error(`  output: ${execOutput.substring(0, 500)}`);
          process.exit(1);
        }
      } else {
        // Data chunk — send demand for next chunk, then process data
        execOutput += data;
        process.stdout.write(`${ms()} stdout-chunk: ${data.substring(0, 200)}\n`);
        sendDemand(2);
        checkPort();
      }
    } else {
      console.log(`${ms()} ← unexpected response ourReq=${ourReq} state=${state}`);
    }
  }
}

function callExec() {
  state = 'wait_exec';
  const execBody = {
    name: ['exec'],
    args: [{
      cmd: nodeExe,
      args: [
        serverMain,
        '--start-server',
        '--host=127.0.0.1',
        '--port=0',
        '--accept-server-license-terms',
        `--connection-token=${token}`,
        '--without-browser-env-var',
        '--telemetry-level', 'off',
      ],
      env: {
        HOME: '/root',
        PATH: '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
      },
    }],
  };
  // async call: reqId=1, flags=F_ASYNC_REQ (non-stream JSON)
  write(F_ASYNC_REQ, execBody, 1);
}

function callStdout(pid) {
  // source subscription: reqId=2, flags=F_SRC_SUB (JSON+stream, not end)
  // type:"source" required — server checks this to know it's a source stream call
  const body = { name: ['stdout'], type: 'source', args: [pid] };
  write(F_SRC_SUB, body, 2);
  // Send initial demand after a tick (let server register subscription first)
  setTimeout(() => sendDemand(2), 20);
}

function sendDemand(ourReqId) {
  // Pull-stream demand: flags=F_SRC_SUB (JSON+stream), body=null means "give me next item"
  // body=true would mean "abort" — we send null to pull data
  write(F_SRC_SUB, null, ourReqId);
}

function checkPort() {
  const m = execOutput.match(/Extension host agent listening on (\d+)/);
  if (m && !serverPort) {
    serverPort = parseInt(m[1]);
    clearTimeout(ovTimer);
    console.log(`\n${ms()} [PASS] VS Code server listening on port ${serverPort}`);
    console.log(`${ms()} [PASS] Full muxrpc flow works!`);
    child.kill('SIGTERM');
    process.exit(0);
  }
}

// Write launch command
const launchCmd = `set -e ; echo -n ${SENTINEL} >&2 ; REMOTE_CONTAINERS_SOCKETS='[]' REMOTE_CONTAINERS_IPC='' '${nodeExe}' '${serverScript}' ; exit\n`;
console.log(`${ms()} Writing launch command...`);
child.stdin.write(launchCmd);
