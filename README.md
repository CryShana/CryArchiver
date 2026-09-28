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

**Custom quality.** Three flags replace a speed preset's quality targets and keep the rest of the preset. The run then names the preset as custom, for example `MODERATE · custom: video VMAF 90/85, image 80`.
- `--video-vmaf MEAN/FLOOR` is the VMAF mean and 5th-percentile floor each video must reach: mean 80–99, floor from 70 up to the mean. The presets use 93/89 at moderate and 95/91 at slow. Noisy sources encoded with film grain still get 2 off the mean and 2.5 off the floor. `fast` never measures video quality, so it refuses this flag.
- `--image-target N` is the SSIMULACRA2 score each image must reach, 50–95. The presets use 80, 85 and 90 at fast, moderate and slow.
- `--audio-kbps N` is the stereo Opus bitrate, 64–320 in steps of 8. Other channel layouts scale with it (mono gets half), and it applies to the audio tracks inside videos too. A lossy source is still not encoded above its own bitrate.
- Video and image values are rounded to 0.1. What the quality cache learns is kept per target, so a custom run never reuses what a preset run learned.

| Option | Meaning | Default |
|---|---|---|
| `-o, --output DIR` | Write outputs into DIR | next to each source |
| `-s, --speed slow\|moderate\|fast` | Trade speed for quality | moderate |
| `--video-vmaf MEAN/FLOOR` | Video quality target (moderate and slow) | the preset's |
| `--image-target N` | Image quality target (SSIMULACRA2) | the preset's |
| `--audio-kbps N` | Stereo audio bitrate; other layouts scale with it | the preset's |
| `--keep-metadata` | Keep personal metadata and file timestamps | off |
| `--drop-aux` | Convert photos that have gain or depth maps, dropping the extras | off |
| `--overwrite` | Replace existing outputs (never sources) | off |
| `--min-savings PERCENT` | Keep the original unless the output is at least this much smaller (0–90) | 10 |
| `-j, --jobs N` | Files processed in parallel (also videos, which otherwise run one at a time) | half the CPU cores, at most 4 |
| `--ffmpeg PATH` | ffmpeg binary or its folder | from PATH |
| `-q, --quiet` | Only print failures and the summary | off |

Exit codes: `0` everything succeeded or was kept/skipped · `1` at least one file failed · `2` no inputs, a bad option, or a problem with ffmpeg.

## Archive projects

An archive project mirrors one folder tree into another: media is compressed, everything else is copied, and duplicates become small placeholder files. Its state lives in `<destination>/<name>.cryarchive`, so it can stop at any moment, even in a crash, and resumes where it left off.

```
cryarchiver project create D:\Photos E:\Archive     # creates E:\Archive\Photos.cryarchive
cryarchiver E:\Archive\Photos.cryarchive            # opens it in the dashboard
cryarchiver run E:\Archive\Photos.cryarchive        # runs it without the dashboard
```

- The source is never modified, unless you turn on deleting originals (below).
- Don't point the destination at another name for a folder inside the source, such as a bind mount, a drive mounted inside it, or an overlapping network share; those can't always be detected.
- Archived files keep their metadata and timestamps; each folder gets its timestamps once everything in it is archived. Empty folders are mirrored too.
- A file that is still being written waits until it has been still for 30 seconds, and on Windows until no other program holds it open for writing.
- Duplicates are confirmed with a full BLAKE3 before anything is replaced, then become `placeholder_<name>.txt` files that point to the archived copy in the shallowest folder. Files under 64 KiB are always archived normally.
- Cue sheets and playlists wait for the files they name, and are copied with the new names written in. Subtitles and other sidecars keep matching their video's name.
- A source folder that can't be read, or a source drive that isn't there, is an open issue: nothing below it is forgotten or archived again, and the run isn't complete until it can be read.
- A project file opens only from its own archive folder; a copy of it elsewhere is refused.
- Nothing is written or removed through a link (a symbolic link or a junction) in the destination. A project whose file or `.cryarchiver` folder, or a folder in it, is a link doesn't open. A mirrored folder that is a link gets nothing written into it: the files that belong there are open issues until it is a real folder.
- A file that fails stays an open issue until you retry it, copy the original or exclude it in the dashboard (`i`). Files in the destination that the project didn't write are reported as issues until you acknowledge them; they are never deleted.
- With watch mode on, the run keeps going once everything is archived and picks up new files as they appear.
- The performance limit runs in the background: low priority, one job at a time, and at most the chosen share of the CPU.
- `project create` takes the custom quality flags (`--video-vmaf`, `--image-target`, `--audio-kbps`) and stores them in the project. Like a speed change, a change to them applies to files archived from then on; files already archived are not redone. `run` names the custom profile in its summary, and `project dump` shows the stored values.

**Deleting originals** is off by default. It can only be turned on in the dashboard's settings (`s`), by typing `DELETE`. An original is then deleted permanently, with no Recycle Bin, once its archived copy has been read back from disk and matches its BLAKE3. A headless run of such a project also needs `--delete-originals`, or it refuses to start. When the dashboard opens such a project, it archives as usual but deletes nothing until you type `DELETE` again in its settings, once per session.

On Linux, the check that an original is still open in another program is best effort: it isn't available on network or FUSE file systems (NFS, CIFS/SMB, FUSE), or for files you don't own. Close programs that write to the source before deleting originals.

| Command | Meaning |
|---|---|
| `project create SOURCE DESTINATION [--name N] [--speed S] [--video-vmaf MEAN/FLOOR] [--image-target N] [--audio-kbps N] [--watch] [--limit PERCENT]` | Create a project; neither folder may be inside the other |
| `tui PROJECT`, or `PROJECT.cryarchive` alone | Open a project in the dashboard |
| `tui`, or no arguments | Open the start menu: recent projects and a new project form |
| `run PROJECT [--delete-originals]` | Run a project without the dashboard |
| `project dump PROJECT` | Print the project's tables, for debugging |

`tui` and `run` also take `--ffmpeg PATH`.

`run` prints a progress line every few seconds and the log as it happens, then a summary that lists the open issues. It exits with `0` once everything is archived without open issues, `1` with open issues (unacknowledged unknown files in the destination included) or when stopped with Ctrl+C before the end (a watch-mode run stopped with Ctrl+C exits with `0` when it has no open issues), and `2` when the project can't be opened or the delete confirmation doesn't agree. A second Ctrl+C exits at once; the next run resumes the jobs that were running.

`cryarchiver` with no arguments, or `cryarchiver tui`, opens the start menu: the last 10 projects you opened, and a form that creates a new project and checks every field as you type. `cryarchiver --demo` or `cryarchiver tui --demo` shows the dashboard on demo data instead. The list of recent projects is kept in `recent.bin`, next to the executable when that folder is writable, otherwise in `%APPDATA%\CryArchiver` or `~/.config/cryarchiver` (`$XDG_CONFIG_HOME/cryarchiver`); it only changes when you open or create a project, or remove projects from it in the menu.

The dashboard needs a terminal of at least 100×30; press `?` for keys and `q` to quit. Quitting stops running jobs, and the project resumes them on the next open.
