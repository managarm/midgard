#!/usr/bin/env python3
"""Builds midgard's guest files on a Linux host: the guest kernel (vmlinuz), which midgard
boots with midgard-init as the initramfs, and the distributions' disk images.

    build.py build <target>... [-o <dir>]

The targets are `kernel` (guest/kernel.toml and guest/linux.config) and the images in
guest/images/<name>: image.toml says how to bootstrap the root file system, the files in
overlay/ are copied into it, and `provision`, a shell script, runs in a chroot of it (as
root, with the network), e.g., to install packages. An image's files are midgard's
template: <name>.qcow2.zst (its sparse 64 GiB ext4 file system as a qcow2 image, compressed
with zstd) and <name>.toml (the default settings of the VM instances that midgard creates
from it, defaults.toml).

`build` builds targets into the output directory, with <target>.json, which lists their
files with their SHA-256 and what they were built from.

Images are built without root: guest/rootfs.py, which creates an image's file system,
runs as root in a user namespace (unshare --map-root-user --map-auto, which needs
subordinate IDs for the user in /etc/subuid and /etc/subgid), such that the files can have
any owner and the package managers can run the packages' scripts.

Downloads and intermediate files are in $XDG_CACHE_HOME/midgard-build
(~/.cache/midgard-build); the output directory defaults to its dist directory. Needs
Python 3.11, what building Linux needs (make, gcc, flex, bison, bc, the libelf headers)
and, for images, util-linux (unshare, mount), e2fsprogs and zstd.
"""

import argparse
import hashlib
import http.client
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tomllib
import urllib.request
from pathlib import Path

GUEST = Path(__file__).resolve().parent
REPO = GUEST.parent
CACHE = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "midgard-build"
DIST = CACHE / "dist"
IMAGES = GUEST / "images"


class Failure(Exception):
    pass


def log(message):
    print(f"build: {message}", file=sys.stderr, flush=True)


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def require(*tools):
    missing = [tool for tool in tools if shutil.which(tool) is None]
    if missing:
        raise Failure(f"{', '.join(missing)} missing")


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as file:
        while chunk := file.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def download(url, path, sha256):
    """Downloads url to path (unless it is there already) and checks its SHA-256."""
    if path.exists() and file_sha256(path) == sha256:
        return path
    log(f"downloading {url}")
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_name(path.name + ".partial")
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=60) as response, open(partial, "wb") as out:
                shutil.copyfileobj(response, out, 1 << 20)
            break
        except (OSError, http.client.HTTPException) as error:
            if attempt == 2:
                raise Failure(f"cannot download {url}: {error}")
            log(f"retrying {url} ({error})")
    actual = file_sha256(partial)
    if actual != sha256:
        partial.unlink()
        raise Failure(f"{url} has the SHA-256 {actual} instead of {sha256}")
    partial.rename(path)
    return path


def cached(source):
    """Downloads the `url` of a table (with its `sha256`) into the cache, once."""
    name = source["url"].rsplit("/", 1)[1]
    return download(source["url"], CACHE / "downloads" / name, source["sha256"])


def load_toml(path):
    with open(path, "rb") as file:
        return tomllib.load(file)


def targets():
    """The kernel and the images."""
    return ["kernel"] + sorted(path.parent.name for path in IMAGES.glob("*/image.toml"))


def inputs(target):
    """The files that a target is built from (relative to the repository)."""
    files = [GUEST / "build.py"]
    if target == "kernel":
        files += [GUEST / "kernel.toml", GUEST / "linux.config"]
    else:
        files += [GUEST / "mkqcow2.py", GUEST / "rootfs.py"]
        files += sorted(
            path for path in (IMAGES / target).rglob("*") if path.is_symlink() or path.is_file()
        )
    return [path.relative_to(REPO) for path in files]


def inputs_hash(target):
    """Identifies the inputs of a target. (Not the built files: what they contain also
    depends on what the distributions' mirrors serve at the time.)"""
    digest = hashlib.sha256()
    for path in inputs(target):
        full = REPO / path
        data = os.readlink(full).encode() if full.is_symlink() else full.read_bytes()
        digest.update(f"{path}\0{len(data)}\0".encode() + data)
    return digest.hexdigest()[:16]


def git_commit():
    result = subprocess.run(
        ["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True
    )
    return result.stdout.strip() if result.returncode == 0 else None


def write_manifest(out, target, files):
    manifest = {
        "target": target,
        "inputs": inputs_hash(target),
        "commit": git_commit(),
        "files": {name: file_sha256(out / name) for name in files},
    }
    (out / f"{target}.json").write_text(json.dumps(manifest, indent=2) + "\n")


def config_settings(path):
    """The settings of a kernel configuration (`n` for those that are not set)."""
    settings = {}
    for line in path.read_text().splitlines():
        if match := re.fullmatch(r"(CONFIG_\w+)=(.*)", line):
            settings[match[1]] = match[2]
        elif match := re.fullmatch(r"# (CONFIG_\w+) is not set", line):
            settings[match[1]] = "n"
    return settings


def build_kernel(out):
    """Builds vmlinuz: linux.config on top of tinyconfig (out of tree)."""
    pin = load_toml(GUEST / "kernel.toml")
    version = pin["version"]
    tarball = cached(pin)
    source = CACHE / f"linux-{version}"
    if not (source / "Makefile").exists():
        log(f"unpacking Linux {version}")
        partial = CACHE / f"linux-{version}.partial"
        shutil.rmtree(partial, ignore_errors=True)
        partial.mkdir(parents=True)
        run("tar", "-xf", tarball, "-C", partial, "--strip-components=1")
        partial.rename(source)

    log(f"building Linux {version}")
    build = CACHE / f"linux-{version}-build"
    build.mkdir(exist_ok=True)
    make = ["make", "-C", source, f"O={build}"]
    env = dict(os.environ, KBUILD_BUILD_USER="midgard", KBUILD_BUILD_HOST="midgard")
    run(*make, "tinyconfig", env=env, stdout=subprocess.DEVNULL)
    run(
        source / "scripts/kconfig/merge_config.sh",
        "-m",
        "-O",
        build,
        build / ".config",
        GUEST / "linux.config",
        stdout=subprocess.DEVNULL,
    )
    run(*make, "olddefconfig", env=env, stdout=subprocess.DEVNULL)
    # Kconfig drops settings whose dependencies are missing (or that do not exist).
    config = config_settings(build / ".config")
    dropped = [
        f"{name}={value}"
        for name, value in config_settings(GUEST / "linux.config").items()
        if config.get(name, "n") != value
    ]
    if dropped:
        log(f"warning: the kernel configuration lacks {', '.join(dropped)}")
    run(*make, f"-j{os.cpu_count()}", "bzImage", env=env)
    shutil.copyfile(build / "arch/x86/boot/bzImage", out / "vmlinuz")
    return ["vmlinuz"]


def fetch_downloads(table):
    """Downloads what a table of image.toml (and its subtables) names (`url`, with its
    `sha256`) and adds the paths of the files (`file`)."""
    if "url" in table:
        table["file"] = str(cached(table))
    for value in table.values():
        if isinstance(value, dict):
            fetch_downloads(value)


def namespace_command():
    """The command that runs rootfs.py as root in a user namespace (in which the user's
    subordinate IDs are the other users and groups), unless we are root."""
    if os.geteuid() == 0:
        return []
    require("unshare")
    command = ["unshare", "--map-root-user", "--map-auto", "--"]
    check = subprocess.run([*command, "true"], capture_output=True, text=True)
    if check.returncode != 0:
        raise Failure(
            f"cannot create a user namespace ({check.stderr.strip()}): unshare --map-auto "
            "needs newuidmap and newgidmap (uidmap) and subordinate IDs for the user in "
            "/etc/subuid and /etc/subgid, and unprivileged user namespaces must be allowed "
            "(Ubuntu restricts them: kernel.apparmor_restrict_unprivileged_userns)"
        )
    return command


def run_terminating(*args):
    """Runs a command; if we are interrupted, we terminate it and wait until it exits (such
    that it can clean up)."""
    with subprocess.Popen([str(arg) for arg in args]) as process:
        try:
            returncode = process.wait()
        except BaseException:
            process.terminate()
            process.wait()
            raise
    if returncode != 0:
        raise subprocess.CalledProcessError(returncode, [str(arg) for arg in args])


def build_image(name, out, namespace):
    """Builds <name>.qcow2.zst and <name>.toml: rootfs.py creates the file system in the
    user namespace."""
    recipe = IMAGES / name
    spec = load_toml(recipe / "image.toml")["bootstrap"]
    fetch_downloads(spec)
    # The work directory is next to the output, as /tmp may be a small tmpfs.
    work = out / f".{name}.work"
    work.mkdir(exist_ok=True)
    ext4 = work / "root.ext4"
    qcow2 = work / "root.qcow2"
    try:
        run_terminating(
            *namespace, sys.executable, GUEST / "rootfs.py", "build", recipe, work, json.dumps(spec)
        )
        log(f"creating {name}.qcow2.zst")
        run(sys.executable, GUEST / "mkqcow2.py", ext4, qcow2)
        ext4.unlink()
        run("zstd", "-q", "-f", "-T0", "-15", qcow2, "-o", out / f"{name}.qcow2.zst")
        shutil.copyfile(recipe / "defaults.toml", out / f"{name}.toml")
    finally:
        ext4.unlink(missing_ok=True)
        qcow2.unlink(missing_ok=True)
        # (Unless a rootfs.py that was killed left the root file system behind.)
        try:
            work.rmdir()
        except OSError:
            pass
    return [f"{name}.qcow2.zst", f"{name}.toml"]


def cmd_build(args):
    out = args.output.resolve()
    for target in args.targets:
        if target not in targets():
            raise Failure(f"there is no target {target} (the targets: {', '.join(targets())})")
    if any(target != "kernel" for target in args.targets):
        require("zstd")
        namespace = namespace_command()
    os.umask(0o022)
    out.mkdir(parents=True, exist_ok=True)
    for target in args.targets:
        if target == "kernel":
            files = build_kernel(out)
        else:
            files = build_image(target, out, namespace)
        write_manifest(out, target, files)
        log(f"built {target}: {', '.join(files)} in {out}")


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True, metavar="command")
    build = commands.add_parser("build", help="build targets")
    build.add_argument("targets", nargs="+", metavar="target")
    build.add_argument("-o", "--output", type=Path, default=DIST, help="the output directory")
    build.set_defaults(handler=cmd_build)
    args = parser.parse_args()

    # (Such that rootfs.py can clean up, see run_terminating(), and subprocess.run() kills
    # the command that it waits for.)
    for signum in [signal.SIGTERM, signal.SIGHUP]:
        signal.signal(signum, lambda signum, frame: sys.exit(128 + signum))
    try:
        args.handler(args)
    except Failure as error:
        sys.exit(f"build: {error}")
    except subprocess.CalledProcessError as error:
        sys.exit(f"build: {' '.join(error.cmd)} failed ({error.returncode})")
    except KeyboardInterrupt:
        sys.exit(130)


if __name__ == "__main__":
    main()
