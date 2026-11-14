const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const input = name => (process.env[`INPUT_${name.toUpperCase()}`] || '').trim();

async function main() {
  const directory = input('defects');
  const token = input('token');
  const run = input('run-url');
  const renderer = input('renderer');
  const defaultReproduction = input('default-reproduction');
  if (!directory || !token || !run || !renderer || !defaultReproduction) throw new Error('Missing action input');
  const results = JSON.parse(input('results'));
  const reproductions = JSON.parse(input('reproductions'));
  const rendererArgs = JSON.parse(input('renderer-args'));
  if (!Array.isArray(rendererArgs)) throw new Error('Invalid renderer arguments');
  const base = `${process.env.GITHUB_API_URL || 'https://api.github.com'}/repos/${process.env.GITHUB_REPOSITORY}`;
  const headers = { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json', 'Content-Type': 'application/json' };
  async function request(endpoint, method = 'GET', body) {
    const response = await fetch(`${base}${endpoint}`, { method, headers, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    if (!response.ok) throw new Error(`GitHub API ${method} ${endpoint}: ${response.status} ${await response.text()}`);
    return response.status === 204 ? null : response.json();
  }
  try { await request('/labels', 'POST', { name: 'fuzz', description: 'Found by the nightly fuzzers', color: 'd93f0b' }); } catch { /* label may already exist */ }
  const issues = [];
  for (let page = 1; issues.length < 500; page++) {
    const batch = await request(`/issues?state=open&per_page=100&page=${page}`);
    issues.push(...batch.filter(issue => !issue.pull_request));
    if (batch.length < 100) break;
  }
  const findIssue = needle => issues.find(issue => issue.title.includes(needle));
  async function closeIssue(issue, comment) {
    await request(`/issues/${issue.number}/comments`, 'POST', { body: comment });
    await request(`/issues/${issue.number}`, 'PATCH', { state: 'closed' });
  }

  if (fs.existsSync(renderer)) fs.chmodSync(renderer, 0o755);
  const files = fs.existsSync(directory) ? fs.readdirSync(directory).sort() : [];
  const filed = new Set();
  const jobsWithDefects = new Set();
  for (const name of files.filter(name => name.endsWith('.json'))) {
    const file = path.join(directory, name);
    const record = JSON.parse(fs.readFileSync(file, 'utf8'));
    jobsWithDefects.add(record.job);
    const rendered = execFileSync(renderer, [...rendererArgs, file], { encoding: 'utf8' });
    const newline = rendered.indexOf('\n');
    if (newline < 0) throw new Error(`Invalid rendered issue: ${file}`);
    const title = rendered.slice(0, newline);
    const signature = title.slice(title.lastIndexOf('[') + 1).replace(/\]$/, '');
    filed.add(signature);
    if (findIssue(`[${signature}]`)) {
      console.log(`already tracked: ${title}`);
    } else {
      await request('/issues', 'POST', { title, body: rendered.slice(newline + 1).trimEnd(), labels: ['bug', 'fuzz'] });
    }
  }

  for (const name of files.filter(name => name.startsWith('fixed') && name.endsWith('.txt'))) {
    const signatures = fs.readFileSync(path.join(directory, name), 'utf8').split(/\r?\n/);
    for (const signature of signatures) {
      if (!signature || filed.has(signature)) continue;
      const issue = findIssue(`[${signature}]`);
      if (issue) await closeIssue(issue, `No longer reproduces as of ${run}.`);
    }
  }

  for (const [job, { result }] of Object.entries(results)) {
    const title = `Nightly infrastructure failure: ${job}`;
    const issue = findIssue(title);
    if (result === 'failure' && !jobsWithDefects.has(job)) {
      if (issue) {
        await request(`/issues/${issue.number}/comments`, 'POST', { body: `Still failing: ${run}` });
      } else {
        const command = reproductions[job] || defaultReproduction;
        const body = `\`${job}\` failed without recording a defect, so this is a build or\nharness problem rather than a miscompile.\n\n- Run: ${run}\n- Reproduce:\n\`\`\`sh\n${command}\n\`\`\`\n\nCloses automatically once \`${job}\` completes again.`;
        await request('/issues', 'POST', { title, body, labels: ['bug'] });
      }
    } else if (result === 'success' && issue) {
      await closeIssue(issue, `Passing again: ${run}`);
    }
  }
}

main().catch(error => { console.error(`::error::${error.message}`); process.exitCode = 1; });
