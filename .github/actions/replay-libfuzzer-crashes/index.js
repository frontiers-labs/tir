const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync, spawnSync } = require('node:child_process');

const input = name => (process.env[`INPUT_${name.toUpperCase()}`] || '').trim();

async function main() {
  const target = input('target');
  const directory = input('output');
  const extractor = input('extractor');
  const replay = input('replay');
  const token = input('token');
  if (!target || !directory || !extractor || !replay || !token) throw new Error('Missing action input');
  const extractorArgs = JSON.parse(input('extractor-args'));
  const replayArgs = JSON.parse(input('replay-args'));
  if (!Array.isArray(extractorArgs) || !Array.isArray(replayArgs)) throw new Error('Invalid command arguments');
  fs.mkdirSync(directory, { recursive: true });
  const fixed = path.join(directory, `fixed-${target}.txt`);
  fs.writeFileSync(fixed, '');

  const issues = [];
  try {
    for (let page = 1; page <= 5; page++) {
      const url = `${process.env.GITHUB_API_URL || 'https://api.github.com'}/repos/${process.env.GITHUB_REPOSITORY}/issues?state=open&labels=fuzz&per_page=100&page=${page}`;
      const response = await fetch(url, { headers: { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json' } });
      if (!response.ok) throw new Error(`GitHub API: ${response.status}`);
      const rows = await response.json();
      issues.push(...rows.filter(issue => !issue.pull_request).map(issue => ({ body: issue.body || '' })));
      if (rows.length < 100) break;
    }
  } catch (error) {
    console.error(`Could not list tracked issues: ${error.message}`);
    issues.length = 0;
  }

  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'libfuzzer-replay-'));
  try {
    const records = path.join(scratch, 'tracked');
    execFileSync(extractor, [...extractorArgs, records], { input: JSON.stringify(issues), stdio: ['pipe', 'inherit', 'inherit'] });
    for (const name of fs.readdirSync(records).filter(name => name.endsWith('.json')).sort()) {
      const record = JSON.parse(fs.readFileSync(path.join(records, name), 'utf8'));
      if (!record.identity.startsWith(`libfuzzer:${target}:`)) continue;
      const crash = path.join(scratch, 'crash.bin');
      fs.writeFileSync(crash, Buffer.from(record.artifact, record.language === 'base64' ? 'base64' : 'utf8'));
      const signature = name.slice(0, -'.json'.length);
      const result = spawnSync(replay, [...replayArgs, crash], { stdio: 'ignore' });
      if (result.status === 0) {
        console.log(`FIXED ${signature}`);
        fs.appendFileSync(fixed, `${signature}\n`);
      } else {
        console.log(`STILL ${signature}`);
      }
    }
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
}

main().catch(error => { console.error(`::error::${error.message}`); process.exitCode = 1; });
