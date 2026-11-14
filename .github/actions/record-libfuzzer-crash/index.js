const fs = require('node:fs');
const path = require('node:path');

const input = name => (process.env[`INPUT_${name.toUpperCase()}`] || '').trim();

try {
  const target = input('target');
  const artifacts = input('artifacts');
  const log = input('log');
  const directory = input('output');
  const reproduce = input('reproduce');
  if (!target || !artifacts || !log || !directory || !reproduce) throw new Error('Missing action input');
  fs.mkdirSync(directory, { recursive: true });
  const crashes = fs.existsSync(artifacts)
    ? fs.readdirSync(artifacts).filter(name => name.startsWith('crash-'))
      .map(name => ({ name, time: fs.statSync(path.join(artifacts, name)).mtimeMs }))
      .sort((a, b) => b.time - a.time)
    : [];
  if (!crashes.length) {
    console.error(`${target} failed without leaving a crash input`);
  } else {
    const name = crashes[0].name;
    const sha = name.slice('crash-'.length);
    const bytes = fs.readFileSync(path.join(artifacts, name));
    const binary = bytes.some(byte => byte !== 9 && byte !== 10 && byte !== 13 && (byte < 32 || byte > 126));
    const language = binary ? 'base64' : 'text';
    const hint = binary ? '# base64 -d the case above into crash.bin, then:' : '# save the case above verbatim as crash.bin, then:';
    const text = fs.readFileSync(log, 'utf8');
    const lines = text.split('\n');
    const start = lines.findIndex(line => /==ERROR|panicked at|SUMMARY:/.test(line));
    const trace = start < 0 ? '' : lines.slice(start, start + 40).join('\n') +
      (start + 40 < lines.length ? '\n' : '');
    const record = {
      job: 'libfuzzer',
      summary: `Crash in ${target}`,
      identity: `libfuzzer:${target}:${sha}`,
      reproduce: `${hint}\n${reproduce}`,
      details: `\`${target}\` crashed on the input below.\n\nRun: ${input('run-url')}\n\n\`\`\`\n${trace}\`\`\``,
      artifact: bytes.toString(binary ? 'base64' : 'utf8'),
      language,
    };
    fs.writeFileSync(path.join(directory, `${target}-${sha}.json`), `${JSON.stringify(record, null, 2)}\n`);
    console.log(`recorded crash ${sha} in ${target}`);
  }
} catch (error) {
  console.error(`::error::${error.message}`);
  process.exitCode = 1;
}
