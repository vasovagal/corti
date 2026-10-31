#!/usr/bin/env python3
"""Offline tests for the tap writer; no GitHub credentials or external repos."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('publish-homebrew.sh')
RECIPE = 'Formula/vagus.rb'


def git(*args, cwd=None, check=True, env=None):
    return subprocess.run(['git', *args], cwd=cwd, env=env, check=check, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE)


class PublisherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.remote = self.root / 'tap.git'
        git('init', '-q', '--bare', '-b', 'main', str(self.remote))
        self.seed = self.root / 'seed'
        git('clone', '-q', str(self.remote), str(self.seed))
        git('config', 'user.name', 'Test', cwd=self.seed)
        git('config', 'user.email', 'test@example.invalid', cwd=self.seed)
        self.write_recipe(self.seed, '1.2.0')
        (self.seed / 'Casks').mkdir()
        (self.seed / 'Casks/corti.rb').write_text('corti untouched\n')
        git('add', '.', cwd=self.seed)
        git('commit', '-qm', 'seed', cwd=self.seed)
        git('push', '-q', 'origin', 'main', cwd=self.seed)
        self.rendered = self.root / 'rendered.rb'
        self.rendered.write_text(self.recipe('1.3.0'))

    @staticmethod
    def recipe(version):
        return f'class Vagus < Formula\n  version "{version}"\nend\n'

    def write_recipe(self, directory, version):
        path = directory / RECIPE
        path.parent.mkdir(exist_ok=True, parents=True)
        path.write_text(self.recipe(version))

    def run_writer(self, version='1.3.0', extra_env=None):
        env = dict(os.environ, TAP_REMOTE=str(self.remote), TAP_BRANCH='main')
        env.update(extra_env or {})
        return subprocess.run(['bash', str(SCRIPT), version, RECIPE, str(self.rendered)],
                              env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def head(self):
        return git('--git-dir', str(self.remote), 'rev-parse', 'main').stdout.strip()

    def read(self, path):
        return git('--git-dir', str(self.remote), 'show', f'main:{path}').stdout

    def test_update_preserves_other_recipe(self):
        result = self.run_writer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.read(RECIPE), self.recipe('1.3.0'))
        self.assertEqual(self.read('Casks/corti.rb'), 'corti untouched\n')

    def test_noop_and_same_version_mismatch(self):
        self.assertEqual(self.run_writer().returncode, 0)
        head = self.head()
        self.assertEqual(self.run_writer().returncode, 0)
        self.assertEqual(self.head(), head)
        self.rendered.write_text(self.recipe('1.3.0') + '# changed\n')
        self.assertNotEqual(self.run_writer().returncode, 0)
        self.assertEqual(self.head(), head)

    def test_downgrade(self):
        self.rendered.write_text(self.recipe('1.1.9'))
        self.assertIn('refusing downgrade', self.run_writer('1.1.9').stderr)
        self.assertEqual(self.read(RECIPE), self.recipe('1.2.0'))

    def test_invalid_version_and_mismatched_render(self):
        for version in ('1.3.0-rc.1', '01.3.0', '1.3.0+build', '1.3.0;echo bad'):
            self.assertNotEqual(self.run_writer(version).returncode, 0)
        self.assertIn('rendered version does not match', self.run_writer('1.4.0').stderr)
        self.assertEqual(self.read(RECIPE), self.recipe('1.2.0'))

    def wrapper(self, mode):
        bindir = self.root / 'bin'
        bindir.mkdir(exist_ok=True)
        import shutil
        real_git = shutil.which('git')
        hook = bindir / 'git'
        hook.write_text('''#!/bin/bash
if [[ "$1" == '-C' && "${3:-}" == push ]]; then
  if [[ "$MODE" == conflict && ! -e "$MARKER" ]]; then
    touch "$MARKER"
    "$REAL_GIT" -C "$SEED" checkout -q main
    printf 'concurrent update\\n' > "$SEED/Casks/corti.rb"
    "$REAL_GIT" -C "$SEED" add Casks/corti.rb
    "$REAL_GIT" -C "$SEED" commit -qm concurrent
    "$REAL_GIT" -C "$SEED" push -q origin main
  elif [[ "$MODE" == failure ]]; then
    exit 1
  fi
fi
exec "$REAL_GIT" "$@"
''')
        hook.chmod(0o755)
        return dict(PATH=str(bindir) + os.pathsep + os.environ['PATH'], REAL_GIT=real_git,
                    MODE=mode, MARKER=str(self.root / 'injected'), SEED=str(self.seed))

    def test_concurrent_other_product_update(self):
        result = self.run_writer(extra_env=self.wrapper('conflict'))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.read('Casks/corti.rb'), 'concurrent update\n')
        self.assertEqual(self.read(RECIPE), self.recipe('1.3.0'))
        self.assertIn('push failed', result.stderr)

    def test_bounded_push_failure(self):
        result = self.run_writer(extra_env=self.wrapper('failure'))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('after 5 attempts', result.stderr)
        self.assertEqual(self.read(RECIPE), self.recipe('1.2.0'))


if __name__ == '__main__':
    unittest.main()
