#!/usr/bin/env python3
"""Converts a raw disk image into a sparse qcow2 image.

The output is a version 3 image with 64 KiB clusters and 16-bit refcounts (like
`qemu-img convert -O qcow2`); only clusters that contain non-zero bytes are stored.
Holes in the raw image are skipped without reading them.

Usage: mkqcow2.py <raw image> <qcow2 image>
"""

import os
import struct
import sys

CLUSTER_BITS = 16
CLUSTER = 1 << CLUSTER_BITS
L2_ENTRIES = CLUSTER // 8
REFCOUNTS_PER_BLOCK = CLUSTER // 2
COPIED = 1 << 63
ZEROS = bytes(CLUSTER)


def data_clusters(raw, size):
    """Yields (guest cluster index, data) for each cluster with non-zero bytes."""
    fd = raw.fileno()
    offset = 0
    while offset < size:
        try:
            start = os.lseek(fd, offset, os.SEEK_DATA)
        except OSError:
            break  # No more data.
        end = os.lseek(fd, start, os.SEEK_HOLE)
        cluster = start // CLUSTER
        while cluster * CLUSTER < end:
            raw.seek(cluster * CLUSTER)
            data = raw.read(CLUSTER)
            if data.rstrip(b"\0"):
                yield cluster, data.ljust(CLUSTER, b"\0")
            cluster += 1
        offset = cluster * CLUSTER


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.strip().splitlines()[-1])
    source, target = sys.argv[1:]
    size = os.path.getsize(source)
    l1_size = -(-size // (CLUSTER * L2_ENTRIES))
    l1_clusters = -(-l1_size * 8 // CLUSTER)

    with open(source, "rb") as raw:
        clusters = [index for index, _ in data_clusters(raw, size)]
    l2_indices = sorted({index // L2_ENTRIES for index in clusters})

    # Layout: header, refcount table (1 cluster: enough for 32 TiB of file), L1 table,
    # refcount blocks, L2 tables, data.
    refcount_table = CLUSTER
    l1_table = 2 * CLUSTER
    blocks = 1
    while True:
        total = 2 + l1_clusters + blocks + len(l2_indices) + len(clusters)
        needed = -(-total // REFCOUNTS_PER_BLOCK)
        if needed <= blocks:
            break
        blocks = needed
    if blocks > L2_ENTRIES:
        sys.exit("mkqcow2: the image is too large")
    first_block = 2 + l1_clusters
    first_l2 = first_block + blocks
    first_data = first_l2 + len(l2_indices)
    l2_cluster = {index: first_l2 + i for i, index in enumerate(l2_indices)}

    header = struct.pack(
        ">4sIQIIQIIQQIIQQQQII",
        b"QFI\xfb", 3,  # magic, version
        0, 0,  # no backing file
        CLUSTER_BITS, size,
        0,  # no encryption
        l1_size, l1_table,
        refcount_table, 1,
        0, 0,  # no snapshots
        0, 0, 0,  # incompatible, compatible, autoclear features
        4,  # 16-bit refcounts
        104,  # header length (no extensions follow: the rest of the cluster is zero)
    )
    l1 = [0] * l1_size
    for index, cluster in l2_cluster.items():
        l1[index] = (cluster * CLUSTER) | COPIED
    l2 = {index: [0] * L2_ENTRIES for index in l2_indices}

    with open(source, "rb") as raw, open(target, "wb") as out:
        out.truncate(total * CLUSTER)
        out.write(header)
        out.seek(refcount_table)
        out.write(struct.pack(f">{blocks}Q", *((first_block + i) * CLUSTER for i in range(blocks))))
        refcounts = bytearray(blocks * CLUSTER)
        struct.pack_into(f">{total}H", refcounts, 0, *([1] * total))
        out.seek(first_block * CLUSTER)
        out.write(refcounts)
        host = first_data
        for index, data in data_clusters(raw, size):
            out.seek(host * CLUSTER)
            out.write(data)
            l2[index // L2_ENTRIES][index % L2_ENTRIES] = (host * CLUSTER) | COPIED
            host += 1
        assert host == total
        out.seek(l1_table)
        out.write(struct.pack(f">{l1_size}Q", *l1))
        for index, cluster in l2_cluster.items():
            out.seek(cluster * CLUSTER)
            out.write(struct.pack(f">{L2_ENTRIES}Q", *l2[index]))


if __name__ == "__main__":
    main()
