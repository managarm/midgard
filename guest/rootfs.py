#!/usr/bin/env python3
"""Creates the disk of one of midgard's images (guest/images/<name>): it bootstraps the root
file system as image.toml's [bootstrap] table says, copies the overlay into it, runs the
provision script in a chroot of it and creates the file system (mke2fs -d) as image.toml's
[disk] table says.

    rootfs.py build <recipe> <work dir> <image>

build.py runs it as root in a user namespace (unshare --map-root-user --map-auto), such
that the files can have any owner and the package managers can run the packages' scripts.
<image> is image.toml as JSON, in which build.py added the paths of the downloads
(`file`). The root file system is in <work dir>/root while it is built (a build that was
killed leaves it behind; the next one removes it), and the disk is <work dir>/disk.raw.
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
from pathlib import Path

# The size of the images' disks (which are sparse, so this only limits what they can hold).
IMAGE_SIZE = 64 << 30
# Where the partition of disks with a partition table starts (in sectors of 512 bytes).
PARTITION_START = 2048
# The environment of commands in chroots of images.
CHROOT_ENV = {
    "PATH": "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/root",
    "LANG": "C.UTF-8",
    "DEBIAN_FRONTEND": "noninteractive",
}
# The devices in chroots of images (bind mounts of the host's).
CHROOT_DEVICES = ["null", "zero", "full", "random", "urandom", "tty"]


class Failure(Exception):
    pass


def log(message):
    print(f"rootfs: {message}", file=sys.stderr, flush=True)


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def require(*tools):
    missing = [tool for tool in tools if shutil.which(tool) is None]
    if missing:
        raise Failure(f"{', '.join(missing)} missing")


def in_namespaces(*command):
    """Runs a command in mount and PID namespaces of its own, such that what it mounts
    disappears when it exits (and nothing that removes its files can follow its mounts)."""
    run(
        "unshare", "--mount", "--propagation", "private", "--pid", "--fork", "--kill-child",
        "--", *command,
    )  # fmt: skip


def mount_points():
    points = []
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        point = re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), line.split()[4])
        points.append(Path(point))
    return points


def remove_tree(path):
    """Removes a directory, unless something is mounted in it."""
    if not os.path.lexists(path):
        return
    for point in mount_points():
        if point.is_relative_to(path):
            raise Failure(f"{point} is mounted, so {path} cannot be removed")
    shutil.rmtree(path)


def in_image(root, relative):
    """The path of a file of an image, with the symlinks in its directory resolved: as we
    are not in a chroot of the image, they must not lead out of it. (The file itself may be
    a symlink.)"""
    path = root / relative
    directory = Path(os.path.realpath(path.parent))
    if not directory.is_relative_to(root):
        raise Failure(f"the directory of /{relative} leads out of the image")
    return directory / path.name


def write_file(root, relative, data, mode=0o644):
    """Writes a file of an image, replacing what is there (which may be a symlink)."""
    path = in_image(root, relative)
    path.unlink(missing_ok=True)
    path.write_bytes(data)
    os.chmod(path, mode)


def bootstrap(spec, root, work):
    """Creates the root file system of an image as image.toml's [bootstrap] says."""
    method = spec["method"]
    if method == "tarball":
        # The root file system as a tarball (possibly in a subdirectory of it).
        args = ["tar", "-xpf", spec["file"], "-C", root, "--numeric-owner", "--xattrs"]
        args.append("--xattrs-include=*")
        if subdirectory := spec.get("subdirectory"):
            args += [f"--strip-components={len(Path(subdirectory).parts)}", subdirectory]
        run(*args)
    elif method == "debootstrap":
        # Debian's debootstrap (from its source tarball), with the archive keyring (from
        # its package). It runs the packages' scripts in a chroot.
        require("ar", "gpgv", "perl", "wget")
        debootstrap = work / "debootstrap"
        keyring = work / "keyring"
        debootstrap.mkdir()
        keyring.mkdir()
        run("tar", "-xzf", spec["debootstrap"]["file"], "-C", debootstrap, "--strip-components=1")
        package = spec["keyring"]["file"]
        data = subprocess.run(["ar", "p", package, "data.tar.xz"], check=True, capture_output=True)
        run("tar", "-xJf", "-", "-C", keyring, input=data.stdout)
        in_namespaces(
            "env", "LC_ALL=C.UTF-8", f"DEBOOTSTRAP_DIR={debootstrap}", debootstrap / "debootstrap",
            "--arch=amd64", f"--variant={spec['variant']}",
            f"--keyring={keyring}/usr/share/keyrings/debian-archive-keyring.gpg",
            f"--include={','.join(spec['include'])}", spec["suite"], root, spec["mirror"],
        )  # fmt: skip
    elif method == "managarm":
        # Managarm's packages from its xbps repository (`channel`), installed as
        # bootstrap-managarm's managarm-install.py does: with a static xbps, and with its
        # provision-sysroot.py, which sets up the repository (with its key) and the system
        # users. The packages stay unconfigured (their scripts are Managarm programs):
        # Managarm configures them when it boots for the first time.
        require("systemd-sysusers", "xz")
        xbps = work / "xbps"
        scripts = work / "managarm-scripts"
        xbps.mkdir()
        (scripts / "xbps-keys").mkdir(parents=True)
        run("tar", "-xJf", spec["xbps"]["file"], "-C", xbps)
        # (provision-sysroot.py reads the keys next to it, by the names in the repository.)
        provision_sysroot = scripts / "provision-sysroot.py"
        shutil.copyfile(spec["provision-sysroot"]["file"], provision_sysroot)
        key = spec["key"]["url"].rsplit("/", 1)[1]
        shutil.copyfile(spec["key"]["file"], scripts / "xbps-keys" / key)
        apply = [sys.executable, provision_sysroot, "apply", "--root", root]
        run(*apply, "--arch", "x86_64", f"--xbps-{spec['channel']}")
        # (Not into the image's package cache.)
        run(
            xbps / "usr/bin/xbps-install", "-y", "-S", "-U", "-r", root,
            "-C", root / "etc/xbps.d", "-c", work / "xbps-cache", *spec["packages"],
            env=dict(os.environ, XBPS_TARGET_ARCH="x86_64"),
        )  # fmt: skip
        # (After the packages, which ship the users' sysusers.d files.)
        run(*apply, "--base-dirs", "--sysusers")
    else:
        raise Failure(f"there is no bootstrap method {method}")


def install_overlay(overlay, root):
    """Copies an image's overlay into its root file system. The files are owned by root
    and have the mode 755 or 644 (whatever the umask of the checkout was)."""
    for directory, subdirectories, files in os.walk(overlay):
        relative = Path(directory).relative_to(overlay)
        # (A directory of the image may be a symlink, too.)
        target = Path(os.path.realpath(root / relative))
        if not target.is_relative_to(root):
            raise Failure(f"/{relative} leads out of the image")
        target.mkdir(mode=0o755, exist_ok=True)
        for name in subdirectories + files:
            path = Path(directory) / name
            if path.is_symlink():
                link = in_image(root, relative / name)
                link.unlink(missing_ok=True)
                os.symlink(os.readlink(path), link)
            elif path.is_file():
                mode = 0o755 if path.stat().st_mode & 0o100 else 0o644
                write_file(root, relative / name, path.read_bytes(), mode)


def chroot(root, *command):
    """Runs a command as root in a chroot of an image, with /proc, /dev, /run and /tmp."""
    in_namespaces(sys.executable, __file__, "_chroot", root, *command)


def enter_chroot(args):
    """Mounts the API file systems in an image and executes a command in a chroot of it
    (in namespaces of our own, see chroot())."""
    root = args.root

    def mount(*args):
        run("mount", *args)

    # The root is a mount point (pacman checks the free space of the mount points).
    mount("--bind", root, root)
    for directory in ["proc", "dev", "run", "tmp"]:
        (root / directory).mkdir(mode=0o755, exist_ok=True)
    mount("-t", "proc", "proc", root / "proc")
    dev = root / "dev"
    mount("-t", "tmpfs", "-o", "mode=755", "tmpfs", dev)
    for device in CHROOT_DEVICES:
        (dev / device).touch()
        mount("--bind", f"/dev/{device}", dev / device)
    for name, target in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ]:
        os.symlink(target, dev / name)
    (dev / "pts").mkdir()
    mount("-t", "devpts", "-o", "newinstance,ptmxmode=0666", "devpts", dev / "pts")
    (dev / "shm").mkdir()
    mount("-t", "tmpfs", "-o", "mode=1777", "tmpfs", dev / "shm")
    mount("-t", "tmpfs", "-o", "mode=755", "tmpfs", root / "run")
    mount("-t", "tmpfs", "-o", "mode=1777", "tmpfs", root / "tmp")
    os.chroot(root)
    os.chdir("/")
    os.execvpe(args.command[0], args.command, CHROOT_ENV)


def provision(root, script):
    """Runs an image's provision script in a chroot of it, with the host's DNS servers."""
    write_file(root, "etc/resolv.conf", Path("/etc/resolv.conf").read_bytes())
    write_file(root, ".midgard-provision", script.read_bytes())
    try:
        chroot(root, "/bin/sh", "-eu", "/.midgard-provision")
    finally:
        in_image(root, ".midgard-provision").unlink()


def finish(root, name):
    """What every image gets. midgard-init configures the network (it writes
    /etc/resolv.conf) and sets the host name from /etc/hostname."""
    in_image(root, "etc/resolv.conf").unlink(missing_ok=True)
    write_file(root, "etc/hostname", f"{name}\n".encode())
    hosts = (
        f"127.0.0.1\tlocalhost\n127.0.1.1\t{name}\n::1\t\tlocalhost ip6-localhost ip6-loopback\n"
    )
    write_file(root, "etc/hosts", hosts.encode())


def create_disk(spec, root, disk, name):
    """Creates the disk with the root file system as image.toml's [disk] table says: the
    file system (`file-system`, default: ext4) takes all of the disk, or the one partition
    of a GPT if the table has the partition's type (`partition-type`; with its UUID,
    `partition-uuid`, if the table has one)."""
    file_system = spec.get("file-system", "ext4")
    with open(disk, "wb") as file:
        file.truncate(IMAGE_SIZE)
    # The inode tables and the journal are zeroed now (as holes in the sparse file), such
    # that the guest kernel does not initialize them later (which would grow the disk).
    options = ["lazy_itable_init=0", "lazy_journal_init=0"]
    size = []
    if partition_type := spec.get("partition-type"):
        require("sfdisk")
        entry = f"start={PARTITION_START}, type={partition_type}"
        if partition_uuid := spec.get("partition-uuid"):
            entry += f", uuid={partition_uuid}"
        table = f"label: gpt\n{entry}\n"
        run("sfdisk", "-q", disk, input=table.encode(), stdout=subprocess.DEVNULL)
        dump = subprocess.run(["sfdisk", "-J", disk], check=True, capture_output=True, text=True)
        (partition,) = json.loads(dump.stdout)["partitiontable"]["partitions"]
        options.append(f"offset={partition['start'] * 512}")
        size = [f"{partition['size'] * 512 // 1024}k"]
    run(
        "mke2fs", "-q", "-t", file_system, "-b", "4096", "-L", name, "-E", ",".join(options),
        "-d", root, disk, *size,
    )  # fmt: skip


def cmd_build(args):
    recipe = args.recipe.resolve()
    name = recipe.name
    work = args.work.resolve()
    spec = json.loads(args.image)
    require("mount", "tar", "mke2fs")
    root = work / "root"
    scratch = [root, work / "debootstrap", work / "keyring"]
    scratch += [work / "xbps", work / "managarm-scripts", work / "xbps-cache"]
    # (Left behind by a build that was killed.)
    for path in scratch:
        remove_tree(path)
    root.mkdir()
    try:
        log(f"bootstrapping {name}")
        bootstrap(spec["bootstrap"], root, work)
        if (recipe / "overlay").is_dir():
            install_overlay(recipe / "overlay", root)
        if (recipe / "provision").exists():
            log(f"provisioning {name}")
            provision(root, recipe / "provision")
        finish(root, name)

        log(f"creating the disk of {name}")
        create_disk(spec.get("disk", {}), root, work / "disk.raw", name)
    finally:
        # (Signals must not interrupt the cleanup.)
        for signum in [signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
            signal.signal(signum, signal.SIG_IGN)
        for path in scratch:
            remove_tree(path)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
    )
    commands = parser.add_subparsers(dest="command", required=True, metavar="command")
    build = commands.add_parser("build", help="create the disk of an image")
    build.add_argument("recipe", type=Path, help="guest/images/<name>")
    build.add_argument("work", type=Path, help="the work directory")
    build.add_argument("image", help="image.toml as JSON")
    build.set_defaults(handler=cmd_build)
    # (See chroot().)
    enter = commands.add_parser("_chroot")
    enter.add_argument("root", type=Path)
    enter.add_argument("command", nargs=argparse.REMAINDER)
    enter.set_defaults(handler=enter_chroot)
    args = parser.parse_args()

    # mke2fs is in /sbin, which is not in the PATH of users on some distributions.
    os.environ["PATH"] += ":/usr/sbin:/sbin"
    os.umask(0o022)
    # (Such that the root file system is removed, see cmd_build(), and subprocess.run()
    # kills the command that it waits for.)
    for signum in [signal.SIGTERM, signal.SIGHUP]:
        signal.signal(signum, lambda signum, frame: sys.exit(128 + signum))
    try:
        args.handler(args)
    except Failure as error:
        sys.exit(f"rootfs: {error}")
    except subprocess.CalledProcessError as error:
        sys.exit(f"rootfs: {' '.join(error.cmd)} failed ({error.returncode})")
    except KeyboardInterrupt:
        sys.exit(130)


if __name__ == "__main__":
    main()
