#!/usr/bin/env node

const { spawn, execFileSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const os = require('os');
const https = require('https');

const pkg = require('../package.json');
const VERSION = pkg.version;
const REPO = 'pplanel/ShadowPTY';

const PLATFORM_MAP = {
  darwin: 'apple-darwin',
  linux: 'unknown-linux-gnu',
};

const ARCH_MAP = {
  arm64: 'aarch64',
  x64: 'x86_64',
};

function getTargetTriple() {
  const platform = PLATFORM_MAP[os.platform()];
  const arch = ARCH_MAP[os.arch()];

  if (!platform || !arch) {
    process.stderr.write(
      `[ShadowPTY] Unsupported platform: ${os.platform()} (${os.arch()})\n`
    );
    process.exit(1);
  }

  return `${arch}-${platform}`;
}

function downloadBinary(url, destination) {
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        return resolve(downloadBinary(res.headers.location, destination));
      }
      if (res.statusCode !== 200) {
        return reject(
          new Error(`Failed to download binary from ${url} (HTTP ${res.statusCode})`)
        );
      }
      const tmpFile = `${destination}.tmp.${Date.now()}`;
      const fileStream = fs.createWriteStream(tmpFile);

      res.pipe(fileStream);

      fileStream.on('finish', () => {
        fileStream.close(() => {
          fs.rename(tmpFile, destination, (err) => {
            if (err) reject(err);
            else resolve();
          });
        });
      });
    }).on('error', reject);
  });
}

async function ensureBinary() {
  // Allow overriding binary path via SHADOWPTY_BIN for local development / testing
  if (process.env.SHADOWPTY_BIN && fs.existsSync(process.env.SHADOWPTY_BIN)) {
    return process.env.SHADOWPTY_BIN;
  }

  const target = getTargetTriple();
  const cacheDir = path.join(os.homedir(), '.cache', 'shadowpty', `v${VERSION}`);
  fs.mkdirSync(cacheDir, { recursive: true });

  const binPath = path.join(cacheDir, 'shadowpty');

  if (!fs.existsSync(binPath)) {
    const assetName = `shadowpty-${target}`;
    const releaseUrl = `https://github.com/${REPO}/releases/download/v${VERSION}/${assetName}`;

    process.stderr.write(
      `[ShadowPTY] Downloading native binary for ${target} from GitHub Releases...\n`
    );

    await downloadBinary(releaseUrl, binPath);
    fs.chmodSync(binPath, 0o755);

    // On macOS, apply ad-hoc code signature to satisfy Gatekeeper / taskgated
    if (os.platform() === 'darwin') {
      try {
        execFileSync('codesign', ['-s', '-', '-f', binPath], { stdio: 'ignore' });
      } catch (_) {
        // Best effort
      }
    }
  }

  return binPath;
}

async function main() {
  const binPath = await ensureBinary();

  // Forward stdio directly to preserve JSON-RPC communication
  const child = spawn(binPath, process.argv.slice(2), {
    stdio: 'inherit',
    env: process.env,
  });

  child.on('exit', (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
    } else {
      process.exit(code ?? 0);
    }
  });

  child.on('error', (err) => {
    process.stderr.write(`[ShadowPTY] Failed to execute binary: ${err.message}\n`);
    process.exit(1);
  });
}

main().catch((err) => {
  process.stderr.write(`[ShadowPTY] ${err.message}\n`);
  process.exit(1);
});
