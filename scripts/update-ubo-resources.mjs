#!/usr/bin/env node
// Genera src-tauri/crates/blocker/resources/ubo-resources.json a partir de los
// scriptlets de uBlock Origin (los `+js(...)` que usan las listas, incluidos los
// que quitan la publicidad de YouTube).
//
// adblock-rust no sabe leer el formato de módulos ES de uBO, así que se importa
// el módulo con Node y se vuelca cada scriptlet como `Resource` en JSON.
//
// Uso:
//   node scripts/update-ubo-resources.mjs            # última release de uBO
//   node scripts/update-ubo-resources.mjs 1.75.0     # tag concreto

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';

const root = path.resolve(import.meta.dirname, '..');
const outFile = path.join(root, 'src-tauri/crates/blocker/resources/ubo-resources.json');

// Bit de permiso que se exige a los scriptlets `requiresTrust` (trusted-*).
// El blocker da ese bit solo a las listas de uBO y Brave.
const TRUSTED = 1;

async function latestTag() {
  const res = await fetch('https://api.github.com/repos/gorhill/uBlock/releases/latest');
  if (!res.ok) throw new Error(`GitHub API: ${res.status}`);
  return (await res.json()).tag_name;
}

const tag = process.argv[2] ?? (await latestTag());
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'ubo-'));
try {
  const tarball = path.join(work, 'ubo.tar.gz');
  const res = await fetch(`https://github.com/gorhill/uBlock/archive/refs/tags/${tag}.tar.gz`);
  if (!res.ok) throw new Error(`descarga de uBO ${tag}: ${res.status}`);
  fs.writeFileSync(tarball, Buffer.from(await res.arrayBuffer()));
  execFileSync('tar', ['xzf', tarball, '-C', work]);

  const resourcesDir = path.join(work, `uBlock-${tag}`, 'src/js/resources');
  const { builtinScriptlets } = await import(
    pathToFileURL(path.join(resourcesDir, 'scriptlets.js')).href
  );

  const resources = [];
  for (const s of builtinScriptlets) {
    // Los scriptlets ISOLATED necesitan el mundo aislado de una extensión;
    // aquí todo se inyecta en el mundo de la página.
    if (s.world === 'ISOLATED') continue;
    const isFn = s.name.endsWith('.fn');
    resources.push({
      name: s.name,
      aliases: s.aliases ?? [],
      kind: { mime: isFn ? 'fn/javascript' : 'application/javascript' },
      content: Buffer.from(s.fn.toString()).toString('base64'),
      dependencies: s.dependencies ?? [],
      ...(s.requiresTrust ? { permission: TRUSTED } : {}),
    });
  }

  fs.mkdirSync(path.dirname(outFile), { recursive: true });
  fs.writeFileSync(
    outFile,
    JSON.stringify({ ubo_version: tag, resources }, null, 1) + '\n',
  );
  console.log(`uBO ${tag}: ${resources.length} recursos -> ${path.relative(root, outFile)}`);
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
