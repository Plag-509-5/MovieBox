'use strict';

const { timingSafeEqual } = require('node:crypto');
const { existsSync } = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const express = require('express');

const EXAMPLE_TOKEN = 'replace-with-a-long-random-secret';
const RATE_WINDOW_MS = 60_000;
const MAX_TRACKED_CLIENTS = 8_192;
const BACKEND_START_TIMEOUT_MS = 45_000;
const BACKEND_REQUEST_TIMEOUT_MS = 50_000;

function parsePort(name, rawValue, fallback) {
  const value = rawValue ?? fallback;
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) {
    throw new Error(`${name} must be an integer between 1 and 65535`);
  }
  return port;
}

function findBackendBinary() {
  const candidates = process.env.MOVIEBOX_API_BIN
    ? [path.resolve(process.env.MOVIEBOX_API_BIN)]
    : [
        path.resolve('bin/moviebox-api'),
        path.resolve('target/release/moviebox-api'),
        path.resolve(__dirname, 'bin/moviebox-api'),
        path.resolve(__dirname, '../../target/release/moviebox-api'),
      ];
  return candidates.find((candidate) => existsSync(candidate));
}

const host = process.env.HOST || '0.0.0.0';
const publicPort = parsePort('SERVER_PORT/PORT', process.env.SERVER_PORT || process.env.PORT, '8080');
const backendPort = parsePort('RUST_API_PORT', process.env.RUST_API_PORT, '18081');
if (backendPort === publicPort) {
  throw new Error('RUST_API_PORT must differ from the Pterodactyl allocation port');
}

const apiToken = (process.env.API_TOKEN || '').trim();
if (apiToken === EXAMPLE_TOKEN) {
  throw new Error('Replace API_TOKEN with a generated secret in the Pterodactyl environment.');
}
if (!apiToken) {
  console.warn('API_TOKEN is unset; Express API routes are unauthenticated.');
}

const corsOrigins = (process.env.API_CORS_ORIGINS || '*')
  .split(',')
  .map((origin) => origin.trim())
  .filter(Boolean);
const configuredRateLimit = Number.parseInt(process.env.API_RATE_LIMIT_PER_MINUTE || '60', 10);
const rateLimitPerMinute = Number.isInteger(configuredRateLimit) && configuredRateLimit > 0
  ? Math.min(10_000, configuredRateLimit)
  : 60;
const trustProxy = process.env.TRUST_PROXY === 'true';

const app = express();
app.disable('x-powered-by');
if (trustProxy) {
  // Set this only when Pterodactyl is behind a trusted proxy that overwrites X-Forwarded-For.
  app.set('trust proxy', 1);
}

const clientWindows = new Map();
function isWithinRateLimit(ip) {
  const now = Date.now();
  if (clientWindows.size >= MAX_TRACKED_CLIENTS && !clientWindows.has(ip)) {
    for (const [address, window] of clientWindows) {
      if (now - window.startedAt >= RATE_WINDOW_MS) clientWindows.delete(address);
    }
    if (clientWindows.size >= MAX_TRACKED_CLIENTS) return false;
  }

  let window = clientWindows.get(ip);
  if (!window || now - window.startedAt >= RATE_WINDOW_MS) {
    window = { startedAt: now, requests: 0 };
    clientWindows.set(ip, window);
  }
  if (window.requests >= rateLimitPerMinute) return false;
  window.requests += 1;
  return true;
}

function bearerTokenMatches(header) {
  const match = /^Bearer\s+(.+)$/i.exec(header || '');
  if (!match) return false;
  const supplied = Buffer.from(match[1].trim());
  const expected = Buffer.from(apiToken);
  return supplied.length === expected.length && timingSafeEqual(supplied, expected);
}

app.use((req, res, next) => {
  const origin = req.get('Origin');
  const wildcard = corsOrigins.includes('*');
  if (origin && !wildcard && !corsOrigins.includes(origin)) {
    return res.status(403).json({ error: 'origin_not_allowed', message: 'This browser origin is not allowed.' });
  }
  if (origin) {
    res.set('Access-Control-Allow-Origin', wildcard ? '*' : origin);
    res.vary('Origin');
  }
  if (req.method === 'OPTIONS') {
    res.set('Access-Control-Allow-Methods', 'GET, OPTIONS');
    res.set('Access-Control-Allow-Headers', 'Authorization, Content-Type');
    res.set('Access-Control-Max-Age', '600');
    return res.status(204).end();
  }
  next();
});

let backendProcess;
let backendReady = false;
let backendError;
let httpServer;
let stopping = false;
let shutdownPromise;

function backendIsRunning() {
  return Boolean(
    backendProcess
      && backendProcess.exitCode === null
      && backendProcess.signalCode === null
      && !backendError,
  );
}

app.get('/healthz', (_req, res) => {
  const healthy = backendReady && backendIsRunning();
  res.status(healthy ? 200 : 503).json({
    status: healthy ? 'ok' : 'unavailable',
    service: 'moviebox-express-api',
    backend: healthy ? 'ok' : 'down',
  });
});

app.use('/api/v1', (req, res, next) => {
  if (req.method !== 'GET') {
    res.set('Allow', 'GET, OPTIONS');
    return res.status(405).json({ error: 'method_not_allowed', message: 'Only GET and OPTIONS are supported.' });
  }

  const clientIp = req.ip || req.socket.remoteAddress || 'unknown';
  if (!isWithinRateLimit(clientIp)) {
    res.set('Retry-After', '60');
    return res.status(429).json({ error: 'rate_limited', message: 'Request limit reached; retry in a minute.' });
  }

  if (apiToken && !bearerTokenMatches(req.get('Authorization'))) {
    return res.status(401).json({ error: 'unauthorized', message: 'A valid Bearer token is required.' });
  }
  next();
}, async (req, res) => {
  try {
    const target = new URL(`http://127.0.0.1:${backendPort}${req.originalUrl}`);
    const upstream = await fetch(target, {
      method: 'GET',
      headers: { accept: req.get('Accept') || 'application/json' },
      signal: AbortSignal.timeout(BACKEND_REQUEST_TIMEOUT_MS),
    });
    const body = Buffer.from(await upstream.arrayBuffer());
    for (const header of ['content-type', 'cache-control', 'retry-after', 'allow', 'x-content-type-options']) {
      const value = upstream.headers.get(header);
      if (value) res.set(header, value);
    }
    return res.status(upstream.status).send(body);
  } catch (error) {
    console.error(`Rust API request failed: ${error.message}`);
    return res.status(502).json({ error: 'backend_unavailable', message: 'The provider service could not complete the request.' });
  }
});

app.use((_req, res) => {
  res.status(404).json({ error: 'not_found', message: 'No endpoint matches this path.' });
});

function delay(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function waitForBackend() {
  const healthUrl = `http://127.0.0.1:${backendPort}/healthz`;
  const deadline = Date.now() + BACKEND_START_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (backendError) throw backendError;
    if (backendProcess.exitCode !== null || backendProcess.signalCode !== null) {
      throw new Error(`Rust API exited during startup (code ${backendProcess.exitCode}, signal ${backendProcess.signalCode || 'none'})`);
    }
    try {
      const response = await fetch(healthUrl, { signal: AbortSignal.timeout(1_000) });
      if (response.ok) return;
    } catch {
      // The Rust child is still starting; retry until the startup deadline.
    }
    await delay(250);
  }
  throw new Error(`Rust API did not become healthy within ${BACKEND_START_TIMEOUT_MS / 1_000} seconds`);
}

function startBackend(binaryPath) {
  backendProcess = spawn(binaryPath, [], {
    stdio: 'inherit',
    env: {
      ...process.env,
      API_HOST: '127.0.0.1',
      PORT: String(backendPort),
      API_TOKEN: '',
      API_CORS_ORIGINS: '*',
      API_RATE_LIMIT_PER_MINUTE: '10000',
      API_TRUST_PROXY: 'false',
    },
  });
  backendProcess.on('error', (error) => {
    backendError = error;
    console.error(`Could not start Rust API binary: ${error.message}`);
  });
  backendProcess.on('exit', (code, signal) => {
    if (stopping) return;
    backendReady = false;
    console.error(`Rust API stopped unexpectedly (code ${code}, signal ${signal || 'none'}).`);
    process.exitCode = 1;
    if (httpServer) httpServer.close();
  });
}

async function listen() {
  await new Promise((resolve, reject) => {
    httpServer = app.listen(publicPort, host, resolve);
    httpServer.once('error', reject);
  });
  console.log(`Express API listening on http://${host}:${publicPort}`);
  console.log(`Rust provider backend listening privately on 127.0.0.1:${backendPort}`);
}

async function stopBackend() {
  if (!backendIsRunning()) return;
  backendProcess.kill('SIGTERM');
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      if (backendProcess.exitCode === null && backendProcess.signalCode === null) backendProcess.kill('SIGKILL');
      resolve();
    }, 5_000);
    timer.unref();
    backendProcess.once('exit', () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

function shutdown(signal) {
  if (shutdownPromise) return shutdownPromise;
  stopping = true;
  console.log(`Received ${signal}; shutting down Express and Rust API.`);
  shutdownPromise = (async () => {
    if (httpServer) {
      await new Promise((resolve) => httpServer.close(resolve));
    }
    await stopBackend();
  })();
  return shutdownPromise;
}

process.once('SIGINT', () => void shutdown('SIGINT'));
process.once('SIGTERM', () => void shutdown('SIGTERM'));

async function main() {
  if (stopping) return;
  const binaryPath = findBackendBinary();
  if (!binaryPath) {
    throw new Error('Missing bin/moviebox-api. Build it with deploy/pterodactyl/package.sh and upload the Pterodactyl archive.');
  }
  startBackend(binaryPath);
  await waitForBackend();
  if (stopping) return;
  backendReady = true;
  await listen();
}

main().catch(async (error) => {
  console.error(error.message);
  process.exitCode = 1;
  await shutdown('startup failure');
});
