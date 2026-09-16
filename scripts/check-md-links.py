#!/usr/bin/env python3
"""Check the relative links of the repository's Markdown files.

For every `*.md` git tracks or would add (plus any paths given on the command line),
every `[text](target)` and `<target>` link that is not an absolute URL must
resolve: the target file or directory exists relative to the file, and a
`#fragment` names a heading of the target (GitHub's anchor rules: lower
case, spaces to hyphens, punctuation dropped, `-N` suffix on duplicates).
Links inside fenced code blocks are ignored. Exit status is the number of
broken links, capped at 1, so `make linkcheck` fails on the first one.

    scripts/check-md-links.py            # every tracked or untracked-unignored .md
    scripts/check-md-links.py docs/*.md  # just these
"""

import re
import subprocess
import sys
import unicodedata
from pathlib import Path

ROOT = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())

LINK_RE = re.compile(r"(?<!\!)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
AUTOLINK_RE = re.compile(r"<((?:\./|\.\./|/)[^>\s]+)>")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE_RE = re.compile(r"^\s*(```|~~~)")


def github_anchor(text):
    """GitHub's heading → id: strip markup, lower, drop punctuation, spaces → '-'."""
    text = re.sub(r"`([^`]*)`", r"\1", text)  # inline code keeps its text
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)  # links keep their text
    text = re.sub(r"<[^>]+>", "", text)
    text = unicodedata.normalize("NFKC", text).lower()
    out = []
    for ch in text:
        if ch.isalnum() or ch in "-_ ":
            out.append(ch)
        # everything else (punctuation, symbols, emoji) is dropped
    return "".join(out).strip().replace(" ", "-")


def strip_fences(lines):
    """Yield (lineno, line) outside fenced code blocks."""
    fence = None
    for i, line in enumerate(lines, 1):
        m = FENCE_RE.match(line)
        if m:
            tok = m.group(1)
            if fence is None:
                fence = tok
            elif tok == fence:
                fence = None
            continue
        if fence is None:
            yield i, line


def anchors_of(path, cache={}):
    if path in cache:
        return cache[path]
    seen = {}
    ids = set()
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        cache[path] = ids
        return ids
    for _, line in strip_fences(lines):
        m = HEADING_RE.match(line)
        if not m:
            continue
        a = github_anchor(m.group(2))
        n = seen.get(a, 0)
        seen[a] = n + 1
        ids.add(a if n == 0 else f"{a}-{n}")
    # explicit <a name="..."> / id="..." anchors
    text = path.read_text(encoding="utf-8", errors="replace")
    for m in re.finditer(r'(?:name|id)="([^"]+)"', text):
        ids.add(m.group(1))
    cache[path] = ids
    return ids


def check_file(md):
    broken = []
    lines = md.read_text(encoding="utf-8").splitlines()
    for lineno, line in strip_fences(lines):
        targets = LINK_RE.findall(line) + AUTOLINK_RE.findall(line)
        for raw in targets:
            if re.match(r"^[a-z][a-z0-9+.-]*:", raw):  # http:, https:, mailto:, ...
                continue
            target, _, frag = raw.partition("#")
            target = target.strip("<>")
            if target == "":
                dest = md
            elif target.startswith("/"):
                dest = ROOT / target.lstrip("/")
            else:
                dest = (md.parent / target).resolve()
            if not dest.exists():
                broken.append((lineno, raw, "missing file"))
                continue
            if frag:
                if dest.is_dir():
                    dest = dest / "README.md"
                if dest.suffix.lower() != ".md":
                    continue
                if frag not in anchors_of(dest):
                    broken.append((lineno, raw, "missing anchor"))
    return broken


def main(argv):
    if argv:
        files = [Path(a).resolve() for a in argv]
    else:
        # Tracked files plus untracked-but-not-ignored ones, so a document
        # written and not yet added is checked before it is committed.
        tracked = subprocess.check_output(["git", "ls-files", "*.md", "**/*.md"], text=True, cwd=ROOT)
        untracked = subprocess.check_output(
            ["git", "ls-files", "--others", "--exclude-standard", "*.md", "**/*.md"], text=True, cwd=ROOT
        )
        files = sorted({(ROOT / p).resolve() for p in (tracked + untracked).split()})
    total = 0
    for md in files:
        for lineno, raw, why in check_file(md):
            total += 1
            print(f"{md.relative_to(ROOT)}:{lineno}: {why}: {raw}")
    print(f"{len(files)} files, {total} broken links")
    return 1 if total else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
