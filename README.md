# CryArchiver

Makes media files smaller without thinking about encoders. Point it at files or folders: it picks the codec and settings for each file, checks the result, and keeps the original whenever the new file wouldn't be meaningfully smaller.

- Audio (FLAC, WAV, ALAC, MP3, AAC, AC3 and more) becomes Opus.
- Video becomes AV1 in MKV; transparent video becomes VP9 with alpha.
- Images (JPEG, PNG, WebP, TIFF, BMP, HEIC, AVIF, JPEG XL and more) become AVIF, or JPEG XL when very large or above 8 bits per channel.
- Animated GIF, APNG and WebP become animated AVIF.

## Requirements

- ffmpeg **8.1 or newer** with libopus, soxr, libsvtav1 (SVT-AV1 4.0+) and libdav1d.
  - libvmaf measures video quality at the moderate and slow speeds.
  - libvpx-vp9 is needed for transparent video, libjxl for JPEG XL, and libaom-av1 for animated images, video larger than 16384×8704 and `--video-encoder aom`.
  - Windows: the gyan.dev "full" build or a BtbN GPL build. Linux: a BtbN static build or your distribution's ffmpeg.
- `cryarchiver doctor` checks what your ffmpeg supports.

## Build

```
cargo build --release
```

The binary is `target/release/cryarchiver` (`cryarchiver.exe` on Windows).

## Use

```
cryarchiver song.flac                         # writes song.cry.opus next to it
cryarchiver holiday.mp4                       # writes holiday.cry.mkv next to it
cryarchiver photos/                           # every file directly inside photos/
cryarchiver "*.flac" -o compressed/           # wildcards also work in PowerShell and cmd
cryarchiver album/ --speed slow               # slower, higher quality
cryarchiver film.mkv --ss 1:00 --to 2:30      # frame-accurate cut
cryarchiver film.mkv -o clip.mp4              # one input: the extension picks the format
cryarchiver film.mkv -o clip.opus             # only the audio, as Opus
cryarchiver clip.mp4 -o clip.avif             # a looping animated AVIF
cryarchiver recordings/ --merge-audio         # all audio tracks mixed into one
cryarchiver film.mkv --burn-subs=eng          # English subtitles drawn into the picture
cryarchiver film.mkv --strip -f webm          # only video and audio, as WebM
cryarchiver clip.mp4 --max-bitrate 12.5       # for sharing: at most 12.5 Mbit/s
cryarchiver film.mkv --resize 1280:-1         # 1280 pixels wide, aspect ratio kept
```

**What it keeps.** Sources are never modified. Video is deinterlaced, rotated upright, and keeps its colour, HDR10/HLG, every audio track, subtitles, chapters and fonts; Dolby Vision keeps its HDR10/HLG base layer (profile 5 keeps the original). Images are rotated upright and keep their colour profile. Cover art and music tags are kept. Personal metadata (GPS, camera, dates, telemetry) is dropped unless you pass `--keep-metadata`, which also keeps file timestamps.

**Quality.** At `--speed moderate` (the default) and `slow`, each video's setting is found by encoding a few seconds of samples and measuring them with VMAF; the finished video is then measured over its whole length. A video that misses the target is encoded once more with more quality; a miss of at most 0.5 VMAF is written with a warning instead. Images are checked with SSIMULACRA2 and encoded again, at most twice, when they miss. What worked is remembered for similar files. `--speed fast` uses fixed settings and doesn't measure video. Noisy video is denoised and gets AV1 film grain back. The result line shows the CRF and the measured VMAF.

**When the original is kept.** When no setting reaches the target, or the output wouldn't be at least `--min-savings` smaller. A requested output (`--ss/--to`, `-f`, `--strip`, `--burn-subs`, `--merge-audio`, `--resize`, `--max-bitrate`) is always written, with a warning if it misses. RAW, PSD/XCF, CMYK JPEG and multi-page TIFF files are never converted. Motion photos, Samsung Live Focus photos, HEIC photos with an HDR gain map, and photos with other unknown extras are kept unless `--drop-aux` is given; depth maps are dropped with a note. JPEG phone photos with an HDR gain map (Ultra HDR) become AVIF that keeps the gain map, with `--drop-aux` too: HDR in Chrome, Edge and Safari 26, the normal picture in other viewers.

**Other behaviour.**
- One video runs at a time while audio and images run in parallel; `-j N` allows N videos. Measuring uses only cores other files leave idle.
- Damaged videos fail with "source damaged" instead of producing a broken copy.
- An existing output is skipped, so re-running a command is safe. Files that would share an output name (song.flac and song.wav) keep the first; the second is skipped with a note.
- Locked files, files still being written, crashed or stalled tools and a full disk are retried up to three times; a crashed tool retries with half the threads. A quality miss is never retried.
- A plain run writes nothing but its outputs: no settings, cache or log files.

**Custom quality.** These replace the preset's targets and keep the rest; the run then names the preset as custom.
- `--video-vmaf MEAN/FLOOR`: VMAF mean (80–99) and 5th-percentile floor (70 up to the mean). Presets: 93/89 moderate, 95/91 slow. Video with film grain gets 2 off the mean and 2.5 off the floor. Not with `fast`.
- `--image-target N`: SSIMULACRA2 score, 50–95. Presets: 80, 85 and 90 at fast, moderate and slow.
- `--audio-kbps N`: stereo Opus bitrate, 64–320 in steps of 8; other layouts scale with it. A lossy source below it keeps its own bitrate (at least 96 kbps stereo).

| Option | Meaning | Default |
|---|---|---|
| `-o, --output PATH` | Output folder, or a file for a single input | next to each source |
| `-f, --format mkv\|webm\|mp4\|opus\|ogg\|mka\|avif\|jxl` | Output format for every file | automatic |
| `-s, --speed slow\|moderate\|fast` | Trade speed for quality | moderate |
| `-c, --codec latest` | Codec family (`compat` arrives later) | latest |
| `--video-vmaf MEAN/FLOOR` | Video quality target | the preset's |
| `--image-target N` | Image quality target | the preset's |
| `--audio-kbps N` | Stereo audio bitrate | the preset's |
| `--keep-metadata` | Keep personal metadata and timestamps | off |
| `--drop-aux` | Also convert motion photos and other photos with extras, dropping the extras | off |
| `--ss TIME`, `--to TIME` | Cut audio or video: `90`, `1:30` or `00:01:30.500` | whole file |
| `--merge-audio` | Mix all audio tracks into one | off |
| `--burn-subs[=TRACK\|LANG]` | Draw subtitles into the picture | off |
| `--sub-size PERCENT` | Height of burned text subtitles | 5 |
| `--strip` | Keep only the main video and the audio | off |
| `--resize W:H` | Scale video and images; `-1` or an empty side keeps the aspect ratio | off |
| `--max-bitrate MBPS` | Cap the video bitrate, 0.1–1000 (video only, plain runs only) | none |
| `--min-savings PERCENT` | Keep the original unless the output is this much smaller (0–90) | 10 |
| `--overwrite` | Replace existing outputs (never sources) | off |
| `-j, --jobs N` | Files in parallel (videos otherwise run one at a time) | half the cores, at most 4 |
| `--threads N` | Threads per file (bounds sustained CPU use) | automatic |
| `--priority normal\|low\|idle` | Process priority | normal |
| `--cpu-limit PERCENT` | Use at most this share of the CPU (peaks too, where the system allows) | none |
| `--ffmpeg PATH` | ffmpeg binary or its folder | from PATH |
| `-q, --quiet` | Only failures and the summary | off |
| `-v, --verbose` | Warnings as they happen, and a failed tool's last 20 lines | off |

Hidden options (`--help` lists them): `--crf N` encodes video at a fixed CRF without measuring quality, `--video-encoder svt-av1|aom`, `--image-encoder avif|jxl|jxl-lossless`.

An animated AVIF made from a video loops forever and has no sound or subtitles (`--burn-subs` draws them in); free disk space is checked first. `--resize` takes whole pixels, at most 65536 a side.

Exit codes: `0` all succeeded, kept or skipped · `1` at least one file failed · `2` no inputs, a bad option, or an ffmpeg problem.

## Archive projects

A project mirrors one folder tree into another: media is compressed, everything else is copied, and duplicates become small placeholder files. Its state lives in `<destination>/<name>.cryarchive`, so it can stop at any moment, even in a crash, and resumes where it left off.

```
cryarchiver project create D:\Photos E:\Archive     # creates E:\Archive\Photos.cryarchive
cryarchiver E:\Archive\Photos.cryarchive            # opens it in the dashboard
cryarchiver run E:\Archive\Photos.cryarchive        # runs it without the dashboard
```

- The source is never modified unless you turn on deleting originals. Don't point the destination at another name for a folder inside the source (a bind mount, a nested drive, an overlapping share).
- Archived files and folders keep their metadata and timestamps; empty folders are mirrored too.
- A file still being written waits until it has been still for 30 seconds (on Windows, until no program holds it open for writing).
- Duplicates are confirmed with a full BLAKE3 hash, then become `placeholder_<name>.txt` files pointing to the archived copy. Files under 64 KiB are always archived normally.
- Cue sheets and playlists are copied with the new names written in; subtitles and other sidecars keep matching their video.
- An unreadable folder, a missing drive, a failed file or an unknown file in the destination is an open issue until it's resolved in the dashboard (`i`). Nothing is ever deleted from the destination, and nothing is written through a link.
- A file's details (Enter) show how it was encoded: the CRF, the scores, a second encode, grain, sizes and warnings.
- Watch mode keeps the run going and picks up new files as they appear.
- The performance limit runs in the background: low priority, one job at a time, at most the chosen share of the CPU.
- `project create` takes the custom quality flags; the dashboard's settings (`s`) change them later. Changes apply to files archived from then on.

**Deleting originals** is off by default and can only be turned on by typing `DELETE`, in the dashboard's settings or the new-project form. An original is then deleted permanently (no Recycle Bin) once its archived copy has been read back and matches its BLAKE3. A headless run needs `--delete-originals` as well; the dashboard asks for `DELETE` again once per session. An original is never deleted when its archived copy is the original itself, a link or hard link to it, or a cloud placeholder. On Linux the "still open in another program" check is best effort (not on NFS, SMB or FUSE, or for files you don't own), so close programs that write to the source first.

| Command | Meaning |
|---|---|
| `project create SOURCE DESTINATION [--name N] [--speed S] [--video-vmaf MEAN/FLOOR] [--image-target N] [--audio-kbps N] [--watch] [--limit PERCENT]` | Create a project; neither folder may be inside the other |
| `tui PROJECT`, or `PROJECT.cryarchive` alone | Open a project in the dashboard |
| `tui`, or no arguments | Start menu: recent projects and a new-project form |
| `run PROJECT [--delete-originals]` | Run a project without the dashboard |
| `project dump PROJECT` | Print the project's tables, for debugging |

`tui` and `run` also take `--ffmpeg PATH`. `cryarchiver --demo` shows the dashboard on demo data. The recent-projects list is kept in `recent.bin` next to the executable when that folder is writable.

`run` prints progress and the log, then a summary of open issues. It exits `0` when everything is archived without open issues, `1` with open issues or when stopped early with Ctrl+C, and `2` when the project can't be opened. A second Ctrl+C exits at once; the next run resumes.

The dashboard needs a terminal of at least 100×30; `?` shows the keys and `q` quits. Running jobs resume on the next open.
