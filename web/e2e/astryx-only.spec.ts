import {readdir, readFile} from 'node:fs/promises';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {expect, test} from '@playwright/test';

const srcDir = join(dirname(fileURLToPath(import.meta.url)), '..', 'src');
const NATIVE = /<(button|input|select|textarea|form|option|label)[\s>/]/g;

async function sourceFiles(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const entry of await readdir(dir, {withFileTypes: true})) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...(await sourceFiles(p)));
    else if (/\.(tsx|ts|jsx|js)$/.test(entry.name)) out.push(p);
  }
  return out;
}

test('source never renders native form elements, only Astryx components', async () => {
  const files = await sourceFiles(srcDir);
  expect(files.length).toBeGreaterThan(0);
  const hits: string[] = [];
  for (const f of files) {
    const text = await readFile(f, 'utf8');
    for (const m of text.matchAll(NATIVE)) {
      const line = text.slice(0, m.index).split('\n').length;
      hits.push(`${f}:${line} ${m[0]}`);
    }
  }
  expect(hits).toEqual([]);
});

test('every UI import comes from @astryxdesign', async () => {
  const files = await sourceFiles(srcDir);
  const bad: string[] = [];
  for (const f of files) {
    const text = await readFile(f, 'utf8');
    for (const m of text.matchAll(/from\s+'([^']+)'/g)) {
      const mod = m[1];
      const allowed =
        mod.startsWith('@astryxdesign/') || mod.startsWith('./') || mod === 'react' || mod === 'react-dom/client';
      if (!allowed) bad.push(`${f}: ${mod}`);
    }
  }
  expect(bad).toEqual([]);
});
