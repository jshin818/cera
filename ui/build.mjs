// Bundles the UI into dist/ (served by vault-server, embedded into the binary at compile time).
import * as esbuild from 'esbuild';
import { cpSync, mkdirSync, readFileSync, writeFileSync, rmSync, existsSync } from 'node:fs';

const watch = process.argv.includes('--watch');
rmSync('dist', { recursive: true, force: true });
mkdirSync('dist/fonts', { recursive: true });

// Local fonts (no network at runtime): Inter 400-700 and Geist Mono 400/500, latin subset.
const faces = [];
for (const [pkg, family, weights] of [['inter', 'Inter', [400, 500, 600, 700]], ['geist-mono', 'Geist Mono', [400, 500]]]) {
  for (const w of weights) {
    for (const sub of ['latin', 'latin-ext']) {
      const f = `${pkg}-${sub}-${w}-normal.woff2`;
      const src = `node_modules/@fontsource/${pkg}/files/${f}`;
      if (!existsSync(src)) continue;
      cpSync(src, `dist/fonts/${f}`);
      const range = sub === 'latin'
        ? 'U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+0304,U+0308,U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,U+2215,U+FEFF,U+FFFD'
        : 'U+0100-02BA,U+02BD-02C5,U+02C7-02CC,U+02CE-02D7,U+02DD-02FF,U+0304,U+0308,U+0329,U+1D00-1DBF,U+1E00-1E9F,U+1EF2-1EFF,U+2020,U+20A0-20AB,U+20AD-20C0,U+2113,U+2C60-2C7F,U+A720-A7FF';
      faces.push(`@font-face{font-family:"${family}";font-style:normal;font-weight:${w};font-display:swap;src:url(fonts/${f}) format("woff2");unicode-range:${range}}`);
    }
  }
}
writeFileSync('dist/fonts.css', faces.join('\n'));
cpSync('src/index.html', 'dist/index.html');
cpSync('src/styles.css', 'dist/styles.css');

const opts = {
  entryPoints: ['src/main.js'],
  bundle: true,
  format: 'esm',
  target: ['safari16', 'chrome115'],
  outfile: 'dist/app.js',
  minify: !watch,
  sourcemap: watch ? 'inline' : false,
  legalComments: 'none',
  logLevel: 'info',
};
if (watch) {
  const ctx = await esbuild.context(opts);
  await ctx.watch();
} else {
  await esbuild.build(opts);
  const size = readFileSync('dist/app.js').length;
  console.log(`app.js ${(size / 1024).toFixed(0)} KB`);
}
