#!/usr/bin/env python3
"""Publishes the guest files that a run of .github/workflows/guest.yml built as a release
(guest-<date>), for .github/workflows/guest-release.yml: the release has the run's files and
those of the previous guest release for the targets that the run did not build, and
manifest.json, which lists them (see `build.py install`). Old guest releases are deleted.

    publish.py <dir> --run <id>

<dir> has the run's artifacts. Needs gh (with a token that may create releases).
"""

import argparse
import datetime
import json
import subprocess
import sys
from pathlib import Path

from build import RELEASE_PREFIX, WORKFLOW, Failure, read_manifest, require, targets

# The name of the workflow that builds the targets.
WORKFLOW_NAME = "Guest"
# The branch whose builds are published.
RELEASE_BRANCH = "master"
# How many guest releases are kept.
KEEP_RELEASES = 5


def log(message):
    print(f"publish: {message}", file=sys.stderr, flush=True)


def gh(*args):
    return subprocess.run(
        ["gh", *map(str, args)], check=True, stdout=subprocess.PIPE, text=True
    ).stdout


def guest_releases():
    """The tags of the guest releases (of the repository of the checkout), newest first."""
    releases = json.loads(gh("release", "list", "--limit", "1000", "--json", "tagName,createdAt"))
    releases.sort(key=lambda release: release["createdAt"], reverse=True)
    return [r["tagName"] for r in releases if r["tagName"].startswith(RELEASE_PREFIX)]


def publish(directory, run):
    require("gh")
    fields = "conclusion,event,headBranch,headSha,url,workflowName"
    info = json.loads(gh("run", "view", run, "--json", fields))
    if (
        info["workflowName"] != WORKFLOW_NAME
        or info["conclusion"] != "success"
        or info["event"] not in ["push", "workflow_dispatch"]
        or info["headBranch"] != RELEASE_BRANCH
    ):
        raise Failure(
            f"{info['url']} is not a successful run of {WORKFLOW} for {RELEASE_BRANCH} "
            "(pushed or started manually)"
        )
    manifest = {}
    for path in sorted(directory.glob("*.json")):
        if path.name != "manifest.json":
            manifest[path.stem] = read_manifest(directory, path.stem)
    if not manifest:
        raise Failure(f"there are no built targets in {directory}")
    notes = [f"The guest files that {info['url']} built:", ""]
    notes += [f"- {target}: built at {entry['commit']}" for target, entry in manifest.items()]

    releases = guest_releases()
    if releases:
        previous = releases[0]
        old = json.loads(gh("release", "download", previous, "-p", "manifest.json", "-O", "-"))
        # (Targets that the repository no longer has are dropped.)
        for target in sorted(set(old) & set(targets()) - set(manifest)):
            entry = old[target]
            for name in entry["files"]:
                gh("release", "download", previous, "-p", name, "-D", directory, "--clobber")
            (directory / f"{target}.json").write_text(json.dumps(entry))
            manifest[target] = read_manifest(directory, target)
            notes.append(f"- {target}: built at {entry['commit']} (from {previous})")
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")

    # (guest-<date>, then guest-<date>.2 etc.)
    tag = f"{RELEASE_PREFIX}{datetime.date.today():%Y.%m.%d}"
    numbers = [1 for release in releases if release == tag]
    numbers += [
        int(release[len(tag) + 1 :]) for release in releases if release.startswith(f"{tag}.")
    ]
    if numbers:
        tag = f"{tag}.{max(numbers) + 1}"
    files = [directory / name for entry in manifest.values() for name in entry["files"]]
    gh(
        "release", "create", tag, "--target", info["headSha"], "--title", tag,
        "--notes", "\n".join(notes), "--latest=false", *files, directory / "manifest.json",
    )  # fmt: skip
    log(f"published {tag}")
    for old_tag in [tag, *releases][KEEP_RELEASES:]:
        gh("release", "delete", old_tag, "--cleanup-tag", "--yes")
        log(f"deleted {old_tag}")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("directory", type=Path, help="where the run's artifacts are")
    parser.add_argument("--run", required=True, metavar="id", help="the run of the workflow")
    args = parser.parse_args()
    try:
        publish(args.directory.resolve(), args.run)
    except Failure as error:
        sys.exit(f"publish: {error}")
    except subprocess.CalledProcessError as error:
        sys.exit(f"publish: {' '.join(error.cmd)} failed ({error.returncode})")


if __name__ == "__main__":
    main()
