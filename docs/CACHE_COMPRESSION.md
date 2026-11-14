# Cache compression

Cache blobs are stored zstd-compressed (level 1), locally and on the remote.
The code is `crates/engine/src/engine/cache_codec.rs`.

## Format

- A revision is written under `manifest-v2.borsh`. Each artifact's `encoding`
  says how its blob is stored: `Zstd` (entry name `<name>.zst`) or `None`.
- Artifacts heph packs itself (raw, file, view) are always compressed. A
  container output (`TarPath`/`CpioPath`) is scanned when it is 1 MiB or more:
  a tar whose payload is already compressed (an OCI image of gzip layers) is
  stored verbatim and can still be adopted by rename. Anything else is
  compressed.
- `size` in the manifest is the decoded size.
- The spill threshold between `cache.db` and `blobs/` applies to the stored,
  compressed bytes, so more artifacts stay inline.
- The remote stores the local bytes verbatim, under the same names, with a
  `manifest-v2.borsh` remote manifest. Upload and download copy; neither
  encodes nor decodes.

## Compatibility

A heph that predates this reads only `manifest-v1.borsh` and ignores
`encoding`. It never sees a v2 revision, so it rebuilds, and its raw blobs use
names that cannot collide with the `.zst` ones. Both versions can share a cache
directory or a bucket.

- Locally, this version still reads v1 revisions, so an upgrade keeps the cache
  warm. GC deletes both manifests and the blobs each one names.
- On the remote, v1 (gzip) entries are not read. A fallback would add a round
  trip to every miss. The remote is cold once after an upgrade.

## What a compressed blob gives up

It has no O(1) pread and no plain file. Its `CacheArtifact` reports no seekable
reader and no file path, and each consumer falls back:

- A FUSE slot with such an input runs on the OS unpack path.
- A plugin streams the blob instead of opening the file.
- The tar index is built by walking the decoded stream.

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
