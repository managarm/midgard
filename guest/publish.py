#!/usr/bin/env python3
"""Publishes the guest files that a run of .github/workflows/guest.yml built as a release
(guest-<date>), for .github/workflows/guest-release.yml: the release has the run's files and
those of the previous guest release for the targets that the run did not build,
manifest.json, which lists the targets, and channel.json, which makes the release a channel
that `bifrost pull --from` installs from (see build.py).

The site directory gets a channel.json that lists the new release's files (by their URLs):
the workflow deploys it to GitHub Pages, where it is the channel that midgard pulls from by
default (see crates/midgard/src/pull.rs). Once it did, `prune` deletes old guest releases.

    publish.py release <dir> --run <id> --site <site>
    publish.py prune

<dir> has the run's artifacts. Needs gh (with a token that may create and delete releases).
"""

import argparse
import datetime
import json
import shutil
import subprocess
import sys
from pathlib import Path

from build import (
    CHANNEL_FILE,
    RELEASE_PREFIX,
    WORKFLOW,
    Failure,
    read_manifest,
    require,
    targets,
    write_channel,
)

# The name of the workflow that builds the targets.
WORKFLOW_NAME = "Guest"
# The branch whose builds are published.
RELEASE_BRANCH = "master"
# How many guest releases `prune` keeps. (The newest one is what the channel lists once the
# workflow deployed it; the older ones are what a deployment of an older run lists.)
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


def write_site(site, directory, manifest, tag):
    """Writes the site that GitHub Pages serves: channel.json, which lists the files of the
    guest release `tag` by their URLs."""
    repo = json.loads(gh("repo", "view", "--json", "nameWithOwner"))["nameWithOwner"]
    write_channel(directory, manifest, f"https://github.com/{repo}/releases/download/{tag}/")
    site.mkdir(parents=True, exist_ok=True)
    shutil.move(directory / CHANNEL_FILE, site / CHANNEL_FILE)
    log(f"the channel in {site} offers {tag}")


def publish(directory, run, site):
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
    # (The directory also has what lists the targets: manifest.json and channel.json.)
    for path in sorted(directory.glob("*.json")):
        if path.stem in targets():
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
    # midgard cannot boot guests with less, and `bifrost pull` needs to know what the agent
    # speaks.
    if not {"kernel", "agent"} <= set(manifest) or "protocol" not in manifest["agent"]:
        raise Failure(
            "the release would lack the kernel or the agent (with the versions of the guest "
            f"protocol that it speaks): publish a run of {WORKFLOW} that built them (one that "
            "was started manually builds all targets)"
        )
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    # (With names instead of URLs: the release is a channel, too.)
    write_channel(directory, manifest)

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
        directory / CHANNEL_FILE,
    )  # fmt: skip
    log(f"published {tag}")
    write_site(site, directory, manifest, tag)


def prune():
    """Deletes old guest releases."""
    require("gh")
    for tag in guest_releases()[KEEP_RELEASES:]:
        gh("release", "delete", tag, "--cleanup-tag", "--yes")
        log(f"deleted {tag}")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True)
    release = commands.add_parser("release", help="publish a run's files as a guest release")
    release.add_argument("directory", type=Path, help="where the run's artifacts are")
    release.add_argument("--run", required=True, metavar="id", help="the run of the workflow")
    release.add_argument(
        "--site", required=True, type=Path, help="where to write the site for GitHub Pages"
    )
    commands.add_parser("prune", help="delete old guest releases")
    args = parser.parse_args()
    try:
        if args.command == "release":
            publish(args.directory.resolve(), args.run, args.site.resolve())
        else:
            prune()
    except Failure as error:
        sys.exit(f"publish: {error}")
    except subprocess.CalledProcessError as error:
        sys.exit(f"publish: {' '.join(error.cmd)} failed ({error.returncode})")


if __name__ == "__main__":
    main()
