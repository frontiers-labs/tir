const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync, execFileSync } = require('node:child_process');
const test = require('node:test');

const hook = path.join(__dirname, 'no-scripts.js');

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'script-policy-test-'));
  execFileSync('git', ['init', '-q'], { cwd: root });
  function call(hookEvent, toolName, toolInput, id = 'one') {
    const event = { hook_event_name: hookEvent, tool_name: toolName, tool_input: toolInput,
      cwd: root, session_id: 'test', tool_use_id: id };
    return spawnSync(process.execPath, [hook], {
      cwd: root, input: JSON.stringify(event), encoding: 'utf8',
    });
  }
  return { root, call, remove: () => fs.rmSync(root, { recursive: true, force: true }) };
}

test('direct edits reject scripts outside agent directories', () => {
  const { root, call, remove } = fixture();
  try {
    assert.equal(call('PreToolUse', 'Write', { file_path: 'bad.py', content: '' }).status, 2);
    assert.equal(call('PreToolUse', 'Write', { file_path: 'utils/scripts/tool.sh', content: '' }).status, 2);
    assert.equal(call('PreToolUse', 'Write', { file_path: 'tool', content: '#!/usr/bin/env -S uv run --script\n' }).status, 2);
    assert.equal(call('PreToolUse', 'Write', { file_path: '.agents/tool.py', content: '' }).status, 0);
    assert.equal(call('PreToolUse', 'Write', { file_path: 'utils/python/tir/__init__.py', content: '' }).status, 0);
    assert.equal(call('PreToolUse', 'apply_patch', {
      command: '*** Begin Patch\n*** Add File: bad.sh\n+echo bad\n*** End Patch',
    }).status, 2);
    assert.equal(call('PreToolUse', 'apply_patch', {
      command: '*** Begin Patch\n*** Delete File: bad.sh\n*** End Patch',
    }).status, 0);
    assert.equal(spawnSync(process.execPath, [hook], { cwd: root, input: '{', encoding: 'utf8' }).status, 2);
    assert.equal(fs.existsSync(path.join(root, 'bad.py')), false);
  } finally { remove(); }
});

test('shell edits are reported after execution while allowed edits pass', () => {
  const { root, call, remove } = fixture();
  try {
    const event = { command: 'write files' };
    assert.equal(call('PreToolUse', 'Bash', event).status, 0);
    fs.writeFileSync(path.join(root, 'bad.py'), 'print(1)\n');
    assert.equal(call('PostToolUse', 'Bash', event).status, 2);
    fs.unlinkSync(path.join(root, 'bad.py'));

    assert.equal(call('PreToolUse', 'Bash', event, 'two').status, 0);
    fs.mkdirSync(path.join(root, '.agents'));
    fs.writeFileSync(path.join(root, '.agents', 'allowed.sh'), '#!/bin/sh\n');
    assert.equal(call('PostToolUse', 'Bash', event, 'two').status, 0);

    fs.mkdirSync(path.join(root, 'utils', 'scripts'), { recursive: true });
    const utility = path.join(root, 'utils', 'scripts', 'tool.py');
    fs.writeFileSync(utility, 'before\n');
    execFileSync('git', ['add', 'utils/scripts/tool.py'], { cwd: root });
    assert.equal(call('PreToolUse', 'Bash', event, 'three').status, 0);
    fs.writeFileSync(utility, 'after\n');
    assert.equal(call('PostToolUse', 'Bash', event, 'three').status, 2);

    assert.equal(call('PreToolUse', 'Bash', event, 'four').status, 0);
    fs.writeFileSync(path.join(root, 'failed.sh'), 'echo bad\n');
    assert.equal(call('PostToolUseFailure', 'Bash', event, 'four').status, 2);
  } finally { remove(); }
});
