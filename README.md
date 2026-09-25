# CryArchiver

Makes media files smaller without thinking about encoders. Point it at files or folders: it picks the codec and settings for each file, checks the result, and keeps the original whenever the new file wouldn't be meaningfully smaller.

Audio is supported today (FLAC, WAV, ALAC, MP3, AAC, AC3 and more → Opus). Video and images are next.

## Requirements

- ffmpeg **8.1 or newer** with libopus and the soxr resampler.
  - Windows: the gyan.dev "full" build or a BtbN GPL build.
  - Linux: a BtbN static build or your distribution's ffmpeg.
- Run `cryarchiver doctor` to check what your ffmpeg supports.

## Build

```
cargo build --release
```

The binary is `target/release/cryarchiver` (`cryarchiver.exe` on Windows).

## Use

```
cryarchiver song.flac                  # writes song.cry.opus next to it
cryarchiver music/                     # every file directly inside music/
cryarchiver "*.flac" -o compressed/    # wildcards also work in PowerShell and cmd
cryarchiver album/ --speed slow        # slower, higher quality
```

- Sources are never modified.
- An existing output is skipped, so running the same command again is safe.
- Files that would share an output name (song.flac and song.wav) are not merged: the first in name order is compressed, the second is skipped with a note.
- Cover art and common music tags are kept. MusicBrainz IDs and lyrics keep their ID3-style names for now.
- `--keep-metadata` also keeps each file's timestamps.

| Option | Meaning | Default |
|---|---|---|
| `-o, --output DIR` | Write outputs into DIR | next to each source |
| `-s, --speed slow\|moderate\|fast` | Trade speed for quality | moderate |
| `--keep-metadata` | Keep personal metadata and file timestamps | off |
| `--overwrite` | Replace existing outputs (never sources) | off |
| `--min-savings PERCENT` | Keep the original unless the output is at least this much smaller (0–90) | 10 |
| `-j, --jobs N` | Files processed in parallel | half the CPU cores, at most 4 |
| `--ffmpeg PATH` | ffmpeg binary or its folder | from PATH |
| `-q, --quiet` | Only print failures and the summary | off |

Exit codes: `0` everything succeeded or was kept/skipped · `1` at least one file failed · `2` no inputs, or a problem with ffmpeg.
