#!/usr/bin/env python3
"""Build a name-search replica of a big public repo: the exact file tree of
HEAD (every path, real .gitignore files) with empty file contents, committed
to a fresh local git repo. File names, depths, and ignore rules are what
name search sees; contents are not, so this is valid for name search only.

  replica.py clone URL DEST            # blobless clone (trees only, ~20 MB for Chromium), then build
  replica.py manifest DIR NAME DEST    # from DIR/NAME.paths.gz + DIR/NAME-gitignores.tgz

Useful where a full checkout is too slow to download (Chromium is several GB;
its trees alone are ~20 MB).
"""
import gzip, os, shutil, subprocess, sys, tarfile, tempfile, time

def run(*a, **k):
    return subprocess.run(a, check=True, **k)

def manifest_from_clone(url, work):
    run("git", "clone", "-q", "--depth", "1", "--filter=blob:none", "--no-checkout", url, work)
    out = subprocess.run(["git", "-C", work, "ls-tree", "-r", "--full-tree", "HEAD"], check=True, capture_output=True, text=True).stdout
    paths = [l.split("\t", 1)[1] for l in out.splitlines() if l.split()[1] == "blob"]
    run("git", "-C", work, "sparse-checkout", "set", "--no-cone", ".gitignore", "**/.gitignore", capture_output=True)
    run("git", "-C", work, "checkout", "-q", "HEAD")
    ignores = {}
    for p in paths:
        if os.path.basename(p) == ".gitignore":
            f = os.path.join(work, p)
            if os.path.exists(f):
                ignores[p] = open(f, "rb").read()
    return paths, ignores

def manifest_from_files(d, name):
    paths = gzip.open(os.path.join(d, f"{name}.paths.gz"), "rt").read().splitlines()
    ignores = {}
    with tarfile.open(os.path.join(d, f"{name}-gitignores.tgz")) as t:
        for m in t.getmembers():
            if m.isfile():
                ignores[os.path.normpath(m.name)] = t.extractfile(m).read()
    return paths, ignores

def build(paths, ignores, dest):
    t0 = time.time()
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    made = set()
    for p in paths:
        d = os.path.dirname(p)
        if d and d not in made:
            os.makedirs(os.path.join(dest, d), exist_ok=True)
            made.add(d)
        with open(os.path.join(dest, p), "wb") as f:
            f.write(ignores.get(p, b""))
    print(f"{len(paths):,} files, {len(ignores)} .gitignore in {time.time() - t0:.1f}s", flush=True)
    t0 = time.time()
    env = dict(os.environ, GIT_AUTHOR_NAME="replica", GIT_AUTHOR_EMAIL="replica@example.invalid",
               GIT_COMMITTER_NAME="replica", GIT_COMMITTER_EMAIL="replica@example.invalid")
    run("git", "-C", dest, "init", "-q")
    # -f: commit every path from the source tree, even ones its own .gitignore matches.
    run("git", "-C", dest, "add", "-A", "-f", ".", env=env)
    run("git", "-C", dest, "commit", "-q", "-m", "replica", env=env)
    print(f"git repo committed in {time.time() - t0:.1f}s", flush=True)

if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "clone":
        with tempfile.TemporaryDirectory() as tmp:
            paths, ign = manifest_from_clone(sys.argv[2], os.path.join(tmp, "src"))
        build(paths, ign, sys.argv[3])
    elif len(sys.argv) == 5 and sys.argv[1] == "manifest":
        paths, ign = manifest_from_files(sys.argv[2], sys.argv[3])
        build(paths, ign, sys.argv[4])
    else:
        sys.exit(__doc__)
