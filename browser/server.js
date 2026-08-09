// A static file server, because `file://` cannot load an ES module or fetch a `.wasm`.
//
// Twenty lines of node rather than a dependency: this directory has no build step and no
// package to install, and adding one so a demo can be opened would be a strange trade. It
// binds to localhost only and refuses anything outside its own directory — it is a way to look
// at `index.html`, not a way to serve anything to anyone.
//
//     node server.js        →  http://127.0.0.1:8787

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = fileURLToPath(new URL('.', import.meta.url));
const PORT = Number(process.env.PORT) || 8787;

// `.wasm` matters: `WebAssembly.instantiateStreaming` refuses anything else, and the failure
// message does not mention the content type.
const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.wasm': 'application/wasm',
  '.bin': 'application/octet-stream',
  '.md': 'text/markdown; charset=utf-8',
};

const server = createServer(async (request, response) => {
  const url = new URL(request.url, `http://${request.headers.host}`);
  const requested = url.pathname === '/' ? '/index.html' : url.pathname;

  // Resolve, then check containment. Checking the raw path for `..` is the version of this
  // that gets bypassed; checking where it actually landed is the version that does not.
  const path = join(ROOT, normalize(decodeURIComponent(requested)));
  if (!path.startsWith(ROOT.endsWith(sep) ? ROOT : ROOT + sep)) {
    response.writeHead(403).end('outside the served directory');
    return;
  }

  try {
    const body = await readFile(path);
    response.writeHead(200, {
      'content-type': TYPES[extname(path)] || 'application/octet-stream',
      'cache-control': 'no-store',
    });
    response.end(body);
  } catch {
    response.writeHead(404).end('not found');
  }
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`Cairn browser worker → http://127.0.0.1:${PORT}`);
});
