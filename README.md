# CryArchiver

Makes media files smaller without thinking about encoders. Point it at files or folders: it picks the codec and settings for each file, checks the result, and keeps the original whenever the new file wouldn't be meaningfully smaller.

Audio, video and images are supported:
- Audio (FLAC, WAV, ALAC, MP3, AAC, AC3 and more) becomes Opus.
- Video becomes AV1 in MKV. Transparent video becomes VP9 with alpha.
- Photos and pictures (JPEG, PNG, WebP, TIFF, BMP, HEIC, AVIF, JPEG XL and more) become AVIF, or JPEG XL when they are very large or have more than 8 bits per channel.
- Animated GIF, APNG and WebP become animated AVIF.

## Requirements

- ffmpeg **8.1 or newer** with libopus, the soxr resampler, libsvtav1 (SVT-AV1 4.0 or newer) and libdav1d. libvpx-vp9 is needed for transparent video, libaom-av1 for animated images, and libjxl for JPEG XL.
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
cryarchiver holiday.mp4                # writes holiday.cry.mkv next to it
cryarchiver photos/                    # writes IMG_0001.cry.avif and so on
cryarchiver music/                     # every file directly inside music/
cryarchiver "*.flac" -o compressed/    # wildcards also work in PowerShell and cmd
cryarchiver album/ --speed slow        # slower, higher quality
```

- Sources are never modified.
- Video is deinterlaced (or inverse-telecined), rotated upright, and keeps its colour, HDR10/HLG, every audio track, subtitles, chapters and fonts.
  - Personal metadata (GPS, camera and dates) and telemetry streams are dropped unless you pass `--keep-metadata`.
  - Dolby Vision is saved as its HDR10- or HLG-compatible base layer. Profile 5, which has none, keeps the original.
- Images are rotated upright and keep their colour profile. Each encode is checked with SSIMULACRA2 and kept only if it reaches the quality target for the chosen speed.
  - Personal metadata (EXIF, XMP, GPS, IPTC) is dropped unless you pass `--keep-metadata`, which carries it over with the orientation and size corrected.
  - Photos with HDR gain maps or depth maps keep the original; `--drop-aux` converts them without the extra images.
  - RAW camera files, layered files (PSD, XCF), CMYK JPEGs and multi-page TIFFs are never converted.
- One video is encoded at a time while audio and images run in parallel; `-j N` allows N videos at once.
- Damaged or truncated videos fail with "source damaged" instead of producing a broken copy.
- An existing output is skipped, so running the same command again is safe.
- Files that would share an output name (song.flac and song.wav) are not merged: the first in name order is compressed, the second is skipped with a note.
- Cover art and common music tags are kept. MusicBrainz IDs and lyrics keep their ID3-style names for now.
- `--keep-metadata` also keeps each file's timestamps.

| Option | Meaning | Default |
|---|---|---|
| `-o, --output DIR` | Write outputs into DIR | next to each source |
| `-s, --speed slow\|moderate\|fast` | Trade speed for quality | moderate |
| `--keep-metadata` | Keep personal metadata and file timestamps | off |
| `--drop-aux` | Convert photos that have gain or depth maps, dropping the extras | off |
| `--overwrite` | Replace existing outputs (never sources) | off |
| `--min-savings PERCENT` | Keep the original unless the output is at least this much smaller (0–90) | 10 |
| `-j, --jobs N` | Files processed in parallel (also videos, which otherwise run one at a time) | half the CPU cores, at most 4 |
| `--ffmpeg PATH` | ffmpeg binary or its folder | from PATH |
| `-q, --quiet` | Only print failures and the summary | off |

Exit codes: `0` everything succeeded or was kept/skipped · `1` at least one file failed · `2` no inputs, or a problem with ffmpeg.

## Archive dashboard (preview)

`cryarchiver` with no arguments, or `cryarchiver tui`, opens the archive dashboard in the terminal. For now it runs on demo data, so the layout can be tried before whole-folder archiving arrives. It needs a terminal of at least 100×30; press `?` for keys and `q` to quit.
