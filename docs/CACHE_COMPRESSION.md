# Cache compression

Cache blobs are stored zstd-compressed (level 1), locally and on the remote.
The code is `crates/engine/src/engine/cache_codec.rs`.

## Format

- A revision is written under `manifest-v2.borsh`. Each artifact's `encoding`
  says how its blob is stored: `Zstd` (entry name `<name>.zst`) or `None`
  (verbatim, entry name `<name>.raw`). Every name a v2 manifest holds has one
  of the two suffixes.
- A `Zstd` blob is in the zstd seekable format (zstd's
  `contrib/seekable_format`). The decoded bytes are cut into 1 MiB frames,
  each compressed on its own with a checksum. A seek table in a skippable frame
  at the end lists each frame's stored and decoded size. The blob is still one
  valid zstd stream, so `zstd -d` and any streaming decoder read it whole.
  Through the seek table, a read at any offset decodes only the frame holding
  it.
- Artifacts heph packs itself (raw, file, view) are always compressed. A
  container output (`TarPath`/`CpioPath`) is scanned when it is 1 MiB or more:
  a tar whose payload is already compressed (an OCI image of gzip layers) is
  stored verbatim and can still be adopted by rename. Anything else is
  compressed.
- A `tmp` revision (an uncacheable target, `--shell`) is stored verbatim. It
  is read back once, by the process that wrote it, so compressing it would
  only cost CPU.
- `size` in the manifest is the decoded size. `heph clean` and `heph gc`
  report the bytes they free at rest: the stored size of each blob they
  delete, so a compressed blob counts its compressed bytes.
- The spill threshold between `cache.db` and `blobs/` applies to the stored,
  compressed bytes, so more artifacts stay inline.
- The remote stores the local bytes verbatim, under the same names, with a
  `manifest-v2.borsh` remote manifest. Upload and download copy; neither
  encodes nor decodes.

## Compatibility

A heph that predates this reads only `manifest-v1.borsh` and ignores
`encoding`. It never sees a v2 revision, so it rebuilds, and its blobs use
their bare names. A v2 name always ends in `.zst` or `.raw`, and the two
suffixes have one length, so distinct artifacts never share a v2 name. The
only way a bare name could equal a suffixed one is a target with two artifacts
`b` and `b.zst` (or `b.raw`); heph refuses to cache such a target. Both
versions can share a cache directory or a bucket.

- Locally, this version still reads v1 revisions, so an upgrade keeps the cache
  warm. GC deletes both manifests and the blobs each one names.
- It never writes a v1 manifest. When the `in_place` fixpoint copies a v1
  revision to a new key, the copy is a v2 revision: each blob is copied
  verbatim to its `.raw` name, so the v2 manifest names no bare blob.
- A local v1 manifest can name blobs that are not local: an older heph
  mirrored it from the remote and fetched blobs lazily. This version cannot
  fetch them, since the remote holds only v2 names now, so it rebuilds the
  target.
- On the remote, v1 (gzip) entries are not read. A fallback would add a round
  trip to every miss. The remote is cold once after an upgrade.

## What a compressed blob gives up

Its `CacheArtifact` still has a seekable reader, which decodes through the seek
table. A FUSE slot serves the blob lazily. The tar index is built from the
headers and decodes only the frames that hold one. A random read decodes up to
one frame (1 MiB). Because the layer holds no open reader between reads, a
layer with no plain file reads whole 1 MiB frames and keeps them in a
process-wide cache of decoded chunks (32 MiB, least recently used evicted
first, keyed by layer and frame). The kernel's 128 KiB reads of one frame then
decode it once, not eight times. A layer backed by a plain file reads exactly
what was asked.

Two things are lost:

- There is no plain file. `file_path()` is `None`, so a plugin streams the
  blob instead of opening the file.
- The output cannot be adopted into the cache by rename. It is compressed on
  the way in.

A container whose payload is already compressed is stored verbatim and keeps
both.

## Frame size

Frames are 1 MiB of decoded data. Each frame starts with an empty window, so
smaller frames compress worse. A random read decodes a whole frame, so larger
frames make it slower. At 1 MiB the blob is 0.5–1.1% larger than a single zstd
frame of the same data:

| sample | one frame | 1 MiB frames |
|---|---|---|
| Go `.a` archives, 116 MB | 4.58x | 4.56x |
| Rust rlibs, 129 MB | 3.54x | 3.52x |
| native binaries, 59 MB | 2.55x | 2.52x |
| Go source, 21 MB | 3.27x | 3.25x |

Both forms decode at about 1.5–1.7 GB/s in-process.

## Why zstd -1

Measured single-threaded with the CLI tools on real build outputs (macOS,
arm64):

| sample | gzip -1 | gzip -6 | zstd -1 | zstd -3 | lz4 |
|---|---|---|---|---|---|
| Go `.a` archives, 116 MB | 3.94x, 181 MB/s | 4.73x, 51 MB/s | 4.58x, 582 MB/s | 4.86x, 527 MB/s | 3.00x, 443 MB/s |
| Rust rlibs, 129 MB | 3.26x, 158 MB/s | 3.63x, 51 MB/s | 3.53x, 724 MB/s | 3.88x, 513 MB/s | 2.55x |
| native binaries, 59 MB | 2.41x, 110 MB/s | 2.61x, 45 MB/s | 2.53x, 624 MB/s | 2.84x, 432 MB/s | 1.89x |
| Go source, 21 MB | 2.88x | 3.31x | 3.27x | 3.52x | 2.20x |

- zstd -1 compresses about as well as gzip -6 and about 3.5x faster than gzip -1.
- zstd decodes at about 1 GB/s, against about 350 MB/s for gzip.
- lz4 is faster still, but loses 25–35% of the ratio.
- The remote used gzip -6 through miniz_oxide, which is slower than the CLI
  numbers above.
