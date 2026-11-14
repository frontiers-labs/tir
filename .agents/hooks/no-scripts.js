const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

let event;
let root;
const bindings = new Set(['utils/python/tir/__init__.py', 'utils/python/tir/_capi.py', 'utils/python/tests/test_tir.py']);

function allowed(file) {
  return ['.agents', '.codex', '.claude'].some(dir => file === dir || file.startsWith(`${dir}/`)) || bindings.has(file);
}

function isScript(file, content) {
  if (/\.(pyw?|sh|bash|bats)$/i.test(file)) return true;
  const line = content.split(/\r?\n/, 1)[0];
  if (!line.startsWith('#!')) return false;
  const words = line.slice(2).trim().split(/\s+/);
  let command = path.basename(words.shift() || '');
  if (command === 'env') command = path.basename(words.find(word => !word.startsWith('-') && !word.includes('=')) || '');
  return /^(bash|sh|python[\d.]*)$/.test(command) || (command === 'uv' && words.includes('--script'));
}

function relativeFile(file) {
  const absolute = path.resolve(event.cwd || root, file);
  let existing = absolute;
  while (!fs.existsSync(existing)) {
    const parent = path.dirname(existing);
    if (parent === existing) break;
    existing = parent;
  }
  const resolved = path.join(fs.realpathSync(existing), path.relative(existing, absolute));
  const relative = path.relative(root, resolved);
  return relative === '..' || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative) ? null : relative.split(path.sep).join('/');
}

function readHead(file) {
  try {
    const fd = fs.openSync(file, 'r');
    try {
      const bytes = Buffer.alloc(256);
      return bytes.toString('utf8', 0, fs.readSync(fd, bytes, 0, bytes.length, 0));
    } finally { fs.closeSync(fd); }
  } catch (error) {
    if (error.code === 'ENOENT' || error.code === 'EISDIR') return '';
    throw error;
  }
}

function snapshot() {
  const unstaged = execFileSync('git', ['ls-files', '--modified', '--others', '--exclude-standard', '-z'], { cwd: root });
  const staged = execFileSync('git', ['diff', '--cached', '--name-only', '--no-renames', '-z'], { cwd: root });
  const scripts = {};
  const changed = `${unstaged.toString('utf8')}\0${staged.toString('utf8')}`;
  for (const file of new Set(changed.split('\0').filter(Boolean))) {
    if (allowed(file)) continue;
    const full = path.join(root, file);
    if (!fs.existsSync(full) || !fs.statSync(full).isFile()) continue;
    if (isScript(file, readHead(full))) {
      scripts[file] = crypto.createHash('sha256').update(fs.readFileSync(full)).digest('hex');
    }
  }
  return scripts;
}

function snapshotFile() {
  if (!event.session_id || !event.tool_use_id) throw new Error('Missing hook session or tool-call ID');
  const key = crypto.createHash('sha256').update(`${root}\0${event.session_id}\0${event.tool_use_id}`).digest('hex');
  return path.join(os.tmpdir(), `tir-script-policy-${key}.json`);
}

function checkEdit() {
  const tool = event.tool_input || {};
  const edits = [];
  if (typeof tool.file_path === 'string') edits.push({ file: tool.file_path, content: tool.content || tool.new_string || '' });
  if (event.tool_name === 'apply_patch') {
    const patch = Array.isArray(tool.command) ? tool.command.join('\n') : String(tool.command || '');
    let current;
    for (const line of patch.split('\n')) {
      const header = line.match(/^\*\*\* (?:Add|Update) File: (.+)$/) || line.match(/^\*\*\* Move to: (.+)$/);
      if (header) {
        current = { file: header[1], content: '' };
        edits.push(current);
      } else if (line.startsWith('*** Delete File:')) {
        current = null;
      } else if (current && line.startsWith('+#!')) {
        current.content = line.slice(1);
      }
    }
  }
  for (const edit of edits) {
    const file = relativeFile(edit.file);
    if (!file || allowed(file)) continue;
    if (isScript(file, edit.content) || isScript(file, readHead(path.join(root, file)))) {
      throw new Error(`Shell and Python scripts may only be edited in .agents, .codex, or .claude: ${file}`);
    }
  }
}

try {
  event = JSON.parse(fs.readFileSync(0, 'utf8'));
  root = fs.realpathSync(execFileSync('git', ['rev-parse', '--show-toplevel'], { encoding: 'utf8' }).trim());
  if (event.tool_name === 'Bash') {
    const state = snapshotFile();
    if (event.hook_event_name === 'PreToolUse') {
      fs.writeFileSync(state, JSON.stringify(snapshot()), { mode: 0o600 });
    } else if (event.hook_event_name === 'PostToolUse' || event.hook_event_name === 'PostToolUseFailure') {
      const before = JSON.parse(fs.readFileSync(state, 'utf8'));
      fs.unlinkSync(state);
      const after = snapshot();
      const changed = Object.keys(after).filter(file => before[file] !== after[file]);
      if (changed.length) throw new Error(`Shell and Python scripts may only be edited in .agents, .codex, or .claude: ${changed.join(', ')}. Undo these changes.`);
    }
  } else if (event.hook_event_name === 'PreToolUse') {
    checkEdit();
  }
} catch (error) {
  console.error(error.message);
  process.exitCode = 2;
}
