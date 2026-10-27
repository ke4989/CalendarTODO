import { readFileSync, writeFileSync } from 'node:fs';
import { createHash } from 'node:crypto';

// Canonical legal texts are imported verbatim by Vite and combined for both
// installers. Generated formats are checked in so source builds stay offline.
const inputs = ['MIT', 'MICROSOFT_COMPONENT_TERMS', 'THIRD_PARTY_NOTICES'];
const parts = inputs.map(name => readFileSync(`src/lib/legal/${name}.txt`, 'utf8'));
const text = parts.join('\n\n' + '='.repeat(72) + '\n\n');
const escapeRtf = value => [...value].map(character => {
  if (character === '\n') return '\\par\n';
  if ('\\{}'.includes(character)) return `\\${character}`;
  const points = [];
  for (let index = 0; index < character.length; index++) {
    const unit = character.charCodeAt(index);
    points.push(unit > 127 ? `\\u${unit > 32767 ? unit - 65536 : unit}?` : character[index]);
  }
  return points.join('');
}).join('');
const rtf = `{\\rtf1\\ansi\\ansicpg1252\\uc1\\deff0{\\fonttbl{\\f0 Segoe UI;}}\\f0\\fs18\n${escapeRtf(text)}}`;
const template = readFileSync('src-tauri/installer/main.template.wxs', 'utf8');
const outputs = {
  'src-tauri/installer/LICENSES.txt': text,
  'src-tauri/installer/LICENSES.rtf': rtf,
  // Inline RTF avoids the CLI's Path::ends_with(".rtf") conversion bug. Escape
  // Handlebars' opening delimiter through its literal-expression syntax.
  'src-tauri/installer/main.wxs': template.replace('<Text SourceFile="{{license}}" />', `<Text><![CDATA[${rtf.replaceAll('{{', '{{"{{"}}')}]]></Text>`),
  'src-tauri/installer/license-content.json': JSON.stringify({
    sources: Object.fromEntries(inputs.map((name, index) => [name, createHash('sha256').update(parts[index]).digest('hex')])),
    combinedSha256: createHash('sha256').update(text).digest('hex'),
  }, null, 2) + '\n',
};
for (const [path, body] of Object.entries(outputs)) {
  if (process.argv.includes('--check')) {
    if (readFileSync(path, 'utf8') !== body) throw new Error(`License derivation differs: ${path}`);
  } else writeFileSync(path, body);
}
console.log(process.argv.includes('--check') ? 'License derivations match canonical texts' : 'Generated installer license text and RTF');
