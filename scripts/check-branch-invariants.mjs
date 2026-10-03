#!/usr/bin/env node
/**
 * 分支模型门禁：main/stage 硬不变量 + 分支命名卫生 + pre-push 守卫。
 * 参考：/Users/crosery/work_file/geek_main/scripts/check-branch-invariants.mjs
 *
 * 只读 Git 证据：不 fetch、不改 refs、不删分支、不创建提交、不连接远端。
 *
 * 硬不变量：
 *   I1  `stage` 必须包含 `main`：`git merge-base --is-ancestor origin/main origin/stage` 必须为真（即 stage ≥ main）。
 *   I2  `main` 不得领先 `stage`：任何写入 `main` 的提交都必须已经存在于 `stage`（只允许把 stage 快进/合并进 main）。
 *
 * 分支命名规范（分支名不用 `-`，只用 `/` 分层；每段只含小写字母与数字，段内多个词用 `_` 连接）：
 *   main / stage         长期分支，只允许这两条
 *   task/<issue>/<slug> 或 task/<slug>  从 stage 拉出，PR 回 stage，合并后删除
 *   dev/<username>       个人自由开发分支：只做验证、不部署，也不得作为进入 stage 的凭据
 */

import { spawnSync } from 'node:child_process';
import { readFileSync, realpathSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export const INVARIANTS = Object.freeze([
  '`stage` 必须包含 `main`：`git merge-base --is-ancestor origin/main origin/stage` 必须为真（即 stage ≥ main）。',
  '`main` 不得领先 `stage`：任何写入 `main` 的提交都必须已经存在于 `stage`（只允许把 stage 快进/合并进 main）。',
]);
export const LONG_LIVED_BRANCHES = Object.freeze(['main', 'stage']);
export const TASK_BRANCH_RE = /^task\/(?:[0-9]+\/)?[a-z0-9]+(?:_[a-z0-9]+)*$/;
export const DEV_BRANCH_RE = /^dev\/[a-z0-9]+(?:_[a-z0-9]+)*$/;
const ZERO_SHA = /^0{40}$/;
const SHA_RE = /^[a-f0-9]{40}$/;
export const RELEASE_TAG_RE = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-rc\.([1-9]\d*))?$/;
const RELEASE_SHAPED_TAG_RE = /^[vV][0-9]/;

export function classifyBranch(name) {
  if (LONG_LIVED_BRANCHES.includes(name)) return 'long-lived';
  if (name.startsWith('task/') || name.startsWith('task-')) return TASK_BRANCH_RE.test(name) ? 'task' : 'task-malformed';
  if (name.startsWith('dev/') || name.startsWith('dev-')) return DEV_BRANCH_RE.test(name) ? 'personal' : 'personal-malformed';
  return 'unexpected';
}

function git(repo, args) {
  const result = spawnSync('git', ['-c', 'core.hooksPath=/dev/null', ...args], {
    cwd: repo,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    timeout: 20000,
    maxBuffer: 8 * 1024 * 1024,
  });
  if (result.error) throw new Error(`无法执行 git ${args.join(' ')}：${result.error.message}`);
  return result;
}

export function resolveCommit(repo, ref) {
  const result = git(repo, ['rev-parse', '--verify', '--quiet', `${ref}^{commit}`]);
  const value = (result.stdout ?? '').trim();
  return result.status === 0 && SHA_RE.test(value) ? value : null;
}

export function isAncestor(repo, earlier, later) {
  const result = git(repo, ['merge-base', '--is-ancestor', earlier, later]);
  if (result.status === 0) return true;
  if (result.status === 1) return false;
  throw new Error(`无法判定祖先关系（${earlier} → ${later}）：${(result.stderr ?? '').trim()}`);
}

export function resolveBranchEvidence({ repo, branch, requireRemote = false }) {
  const remoteRef = `refs/remotes/origin/${branch}`;
  const localRef = `refs/heads/${branch}`;
  const remote = resolveCommit(repo, remoteRef);
  if (remote) return { branch, ref: remoteRef, sha: remote, scope: 'remote' };
  if (requireRemote) {
    throw new Error(`找不到 ${remoteRef}：判定不变量需要远端证据，请确认 fetch 完整。`);
  }
  const local = resolveCommit(repo, localRef);
  if (local) return { branch, ref: localRef, sha: local, scope: 'local' };
  throw new Error(`找不到 ${remoteRef} 或 ${localRef}：缺少判定不变量所需的 Git 证据。`);
}

export function collectBranches(repo) {
  const result = git(repo, ['for-each-ref', '--format=%(refname)', 'refs/heads', 'refs/remotes/origin']);
  if (result.status !== 0) throw new Error(`无法枚举分支：${(result.stderr ?? '').trim()}`);
  const branches = new Map();
  for (const ref of (result.stdout ?? '').trim().split('\n').filter(Boolean)) {
    let name = null;
    let scope = null;
    if (ref.startsWith('refs/heads/')) {
      name = ref.slice('refs/heads/'.length);
      scope = 'local';
    } else if (ref.startsWith('refs/remotes/origin/')) {
      name = ref.slice('refs/remotes/origin/'.length);
      if (name === 'HEAD') continue;
      scope = 'remote';
    }
    if (!name || !scope) continue;
    const entry = branches.get(name) ?? { name, local: false, remote: false };
    entry[scope] = true;
    branches.set(name, entry);
  }
  return [...branches.values()].sort((left, right) => left.name.localeCompare(right.name));
}

function branchNamingMessage(branch) {
  const kind = classifyBranch(branch.name);
  const where = [branch.remote ? 'origin' : null, branch.local ? '本地' : null].filter(Boolean).join('+') || '未知来源';
  if (kind === 'unexpected') {
    return `分支 ${branch.name}（${where}）不是长期分支 main/stage，也不是 task/<slug> 或 dev/<username>：长期分支只允许 main 与 stage。`;
  }
  if (kind === 'task-malformed') {
    return `分支 ${branch.name}（${where}）不符合 task/<issue>/<slug> 命名（分支名不用 -，只用 / 分层，slug 词间用 _）。`;
  }
  if (kind === 'personal-malformed') {
    return `分支 ${branch.name}（${where}）不符合 dev/<github-username> 命名（分支名不用 -）。`;
  }
  return null;
}

export function checkInvariants({ repo = process.cwd(), requireRemote = false, strictLongLived = false } = {}) {
  const violations = [];
  const warnings = [];
  const notes = [];
  const main = resolveBranchEvidence({ repo, branch: 'main', requireRemote });
  const stage = resolveBranchEvidence({ repo, branch: 'stage', requireRemote });
  notes.push(`证据：main=${main.ref}@${main.sha.slice(0, 12)}，stage=${stage.ref}@${stage.sha.slice(0, 12)}。`);
  if (!isAncestor(repo, main.sha, stage.sha)) {
    violations.push(`${INVARIANTS[0]}\n  证据：${main.sha.slice(0, 12)} 不是 ${stage.sha.slice(0, 12)} 的祖先，stage 缺少 main 的提交。`);
  }
  const aheadResult = git(repo, ['rev-list', '--max-count=50', `${stage.ref}..${main.ref}`]);
  const ahead = aheadResult.status === 0 ? (aheadResult.stdout ?? '').trim().split('\n').filter(Boolean) : [];
  if (ahead.length) {
    violations.push(`${INVARIANTS[1]}\n  证据：main 领先 stage 的提交 ${ahead.map(sha => sha.slice(0, 12)).join(' ')}`);
  }
  for (const branch of collectBranches(repo)) {
    const message = branchNamingMessage(branch);
    if (!message) continue;
    (strictLongLived ? violations : warnings).push(message);
  }
  return { ok: violations.length === 0, violations, warnings, notes, main, stage };
}

function main(argv) {
  const options = { repo: process.cwd(), requireRemote: false, strictLongLived: false, json: false };
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index];
    if (arg === '--help' || arg === '-h') {
      console.log('用法: node scripts/check-branch-invariants.mjs [--require-remote-refs] [--strict-long-lived] [--json]');
      return;
    }
    if (arg === '--require-remote-refs') options.requireRemote = true;
    else if (arg === '--strict-long-lived') options.strictLongLived = true;
    else if (arg === '--json') options.json = true;
    else if (arg === '--repo') {
      options.repo = resolve(argv[++index]);
    }
  }
  const result = checkInvariants(options);
  if (options.json) {
    console.log(JSON.stringify(result, null, 2));
  } else {
    for (const note of result.notes ?? []) console.log(`[证据] ${note}`);
    for (const warning of result.warnings) console.log(`[警告] ${warning}`);
    if (!result.ok) {
      console.log('[不变量原文]');
      INVARIANTS.forEach((text, i) => console.log(`  ${i + 1}. ${text}`));
      for (const v of result.violations) console.log(`[违规] ${v}`);
    }
  }
  if (result.ok) {
    console.log('分支不变量通过：stage ≥ main，且没有 main 领先 stage 的提交。');
    return;
  }
  process.exitCode = 1;
}

if (process.argv[1] && pathToFileURL(realpathSync(resolve(process.argv[1]))).href === import.meta.url) {
  try {
    main(process.argv.slice(2));
  } catch (err) {
    console.error(err.message);
    process.exitCode = 1;
  }
}
