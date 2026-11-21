import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join } from 'node:path';
export function sourceIdentity(root) {
  const git = args => execFileSync('git', args, {cwd:root,encoding:'utf8'}).trim();
  const names = git(['ls-files','--cached','--others','--exclude-standard']).split('\n').filter(name =>
    /^(src\/|electron\/|rust-core\/src\/|assets\/|public\/|scripts\/)/.test(name) || /^(LICENSE|package(-lock)?\.json|index\.html|vite\.config\.ts|tsconfig.*\.json|rust-core\/Cargo\.(toml|lock))$/.test(name)
  ).sort();
  const hash = createHash('sha256');
  for (const name of names) hash.update(name+'\0').update(readFileSync(join(root,name))).update('\0');
  return { commit:git(['rev-parse','HEAD']), inputsSha256:hash.digest('hex') };
}
