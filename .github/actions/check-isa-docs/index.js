const fs = require('node:fs');
const path = require('node:path');

const input = name => (process.env[`INPUT_${name.toUpperCase()}`] || '').trim();
const read = file => fs.readFileSync(file, 'utf8');

function files(directory, extension) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) return files(file, extension);
    return entry.isFile() && file.endsWith(extension) ? [file] : [];
  });
}

function parseSvg(svg) {
  const stack = [];
  let root;
  let position = 0;
  for (const match of svg.matchAll(/<[^>]*>/g)) {
    const text = svg.slice(position, match.index);
    if (/[<&]/.test(text.replace(/&(?:amp|lt|gt|quot|apos|#\d+|#x[\da-fA-F]+);/g, ''))) {
      throw new Error('Invalid SVG text');
    }
    if (stack.length) stack.at(-1).text += text;
    if (match[0].startsWith('<!--') && match[0].endsWith('-->')) {
      position = match.index + match[0].length;
      continue;
    }
    const tag = match[0].match(/^<\s*(\/?)\s*([\w:.-]+)([\s\S]*?)\s*(\/?)>$/);
    if (!tag) throw new Error(`Invalid SVG tag: ${match[0]}`);
    const [, closing, name, attributes, selfClosing] = tag;
    if (closing) {
      if (attributes.trim() || selfClosing || stack.pop()?.name !== name) throw new Error(`Invalid SVG closing tag: ${name}`);
    } else {
      const attrs = {};
      const attrPattern = /([\w:.-]+)\s*=\s*("[^"]*"|'[^']*')/g;
      let end = 0;
      for (const attr of attributes.matchAll(attrPattern)) {
        const separator = attributes.slice(end, attr.index);
        if (!separator || separator.trim()) throw new Error(`Invalid SVG attributes: ${name}`);
        if (Object.hasOwn(attrs, attr[1]) || /[<&]/.test(attr[2].replace(/&(?:amp|lt|gt|quot|apos|#\d+|#x[\da-fA-F]+);/g, ''))) {
          throw new Error(`Invalid SVG attribute: ${attr[1]}`);
        }
        attrs[attr[1]] = attr[2].slice(1, -1);
        end = attr.index + attr[0].length;
      }
      if (attributes.slice(end).trim()) throw new Error(`Invalid SVG attributes: ${name}`);
      const node = { name, attrs, text: '', children: [] };
      if (stack.length) stack.at(-1).children.push(node);
      else if (root) throw new Error('Multiple SVG roots');
      else root = node;
      if (!selfClosing) stack.push(node);
    }
    position = match.index + match[0].length;
  }
  if (stack.length || !root || root.name !== 'svg' || svg.slice(position).trim()) throw new Error('Invalid SVG document');
  return root;
}

function checkDiagram(svg, file) {
  const label = svg.attrs['aria-label'] || '';
  if (!label.endsWith('-bit instruction encoding')) return false;
  if (svg.attrs.xmlns !== 'http://www.w3.org/2000/svg') throw new Error(`${file}: invalid SVG namespace`);
  const bits = Number(label.slice(0, -'-bit instruction encoding'.length));
  const viewBox = svg.attrs.viewBox || svg.attrs.viewbox;
  if (!Number.isInteger(bits) || bits <= 0 || !viewBox) throw new Error(`${file}: invalid diagram dimensions`);
  const width = Number(viewBox.split(/\s+/)[2]) - 2;
  const bitWidth = width / bits;
  let nextBit = bits - 1;
  for (const field of svg.children.filter(child => child.name === 'g')) {
    const title = field.children.find(child => child.name === 'title')?.text || '';
    const match = title.match(/^Bits (\d+):(\d+): (.+)$/);
    if (!match) throw new Error(`${file}: missing field description: ${title}`);
    const end = Number(match[1]);
    const start = Number(match[2]);
    if (end !== nextBit || start > end) throw new Error(`${file}: gap or overlap at ${title}`);
    const rect = field.children.find(child => child.name === 'rect');
    if (!rect || Number(rect.attrs.width) !== (end - start + 1) * bitWidth) {
      throw new Error(`${file}: field width is not proportional at ${title}`);
    }
    if (Number(rect.attrs.x) !== 1 + (bits - 1 - end) * bitWidth) {
      throw new Error(`${file}: field position is wrong at ${title}`);
    }
    nextBit = start - 1;
  }
  if (nextBit !== -1) throw new Error(`${file}: incomplete ${bits}-bit diagram`);
  return true;
}

try {
  process.chdir(process.env.GITHUB_WORKSPACE);
  const sources = input('sources');
  const book = input('book');
  const targets = JSON.parse(input('targets'));
  const withoutEncodings = new Set(input('without-encodings').split('\n').map(line => line.trim()).filter(Boolean));
  const chapters = files(sources, '.md');
  if (!chapters.length) throw new Error('No generated ISA chapters found');
  for (const chapter of chapters) {
    const html = path.join(book, path.relative(sources, chapter).replace(/\.md$/, '.html'));
    if (!fs.existsSync(html)) throw new Error(`ISA chapter missing from rendered book: ${html}`);
  }

  let declarations = 0;
  let diagrams = 0;
  for (const [target, sourceDirectory] of Object.entries(targets)) {
    const bookDirectory = path.join(book, target);
    const overview = read(path.join(bookDirectory, 'index.html'));
    for (const file of files(sourceDirectory, '.tmdl')) {
      for (const declaration of read(file).matchAll(/^(isa|register_class) (\w+)/gm)) {
        const [, kind, name] = declaration;
        const section = overview.match(new RegExp(`<h3\\b[^>]*>.*?<code>${name}</code>.*?</h3>(.*?)(?=<h[23]\\b|$)`, 's'));
        if (!section) throw new Error(`${file}: ${kind} ${name} missing from ${bookDirectory}/index.html`);
        if (!/<p>.*?<a href="https:\/\//s.test(section[1])) throw new Error(`${file}: ${kind} ${name} has no rendered manual link`);
        declarations++;
      }
    }
    let targetDiagrams = 0;
    for (const entry of fs.readdirSync(bookDirectory, { withFileTypes: true })) {
      if (!entry.isFile() || !entry.name.endsWith('.html')) continue;
      const file = path.join(bookDirectory, entry.name);
      for (const match of read(file).matchAll(/<svg\b[\s\S]*?<\/svg>/g)) {
        if (checkDiagram(parseSvg(match[0]), file)) targetDiagrams++;
      }
    }
    if (withoutEncodings.has(target) ? targetDiagrams !== 0 : targetDiagrams === 0) {
      throw new Error(`${bookDirectory}: ${withoutEncodings.has(target) ? 'unexpected instruction diagrams' : 'no instruction diagrams rendered'}`);
    }
    diagrams += targetDiagrams;
  }
  console.log(`All ${chapters.length} generated ISA chapters were rendered.`);
  console.log(`Checked ${declarations} documented declarations and ${diagrams} instruction diagrams.`);
} catch (error) {
  console.error(`::error::${error.message}`);
  process.exitCode = 1;
}
