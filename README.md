# CryArchiver

Makes media files smaller without thinking about encoders. Point it at files or folders: it picks the codec and settings for each file, checks the result, and keeps the original whenever the new file wouldn't be meaningfully smaller.

Audio, video and images are supported:
- Audio (FLAC, WAV, ALAC, MP3, AAC, AC3 and more) becomes Opus.
- Video becomes AV1 in MKV. Transparent video becomes VP9 with alpha.
- Photos and pictures (JPEG, PNG, WebP, TIFF, BMP, HEIC, AVIF, JPEG XL and more) become AVIF, or JPEG XL when they are very large or have more than 8 bits per channel.
- Animated GIF, APNG and WebP become animated AVIF.

## Requirements

- ffmpeg **8.1 or newer** with libopus, the soxr resampler, libsvtav1 (SVT-AV1 4.0 or newer) and libdav1d. libvpx-vp9 is needed for transparent video, libaom-av1 for animated images, video larger than 16384×8704 and `--video-encoder aom` (without it those files fail), and libjxl for JPEG XL.
  libvmaf (part of most full builds) measures video quality at the moderate and slow speeds.
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
cryarchiver song.flac                         # writes song.cry.opus next to it
cryarchiver holiday.mp4                       # writes holiday.cry.mkv next to it
cryarchiver photos/                           # writes IMG_0001.cry.avif and so on
cryarchiver music/                            # every file directly inside music/
cryarchiver "*.flac" -o compressed/           # wildcards also work in PowerShell and cmd
cryarchiver album/ --speed slow               # slower, higher quality
cryarchiver film.mkv --ss 1:00 --to 2:30      # frame-accurate cut
cryarchiver film.mkv -o clip.mp4              # one input: -o can name the file, its extension picks the format
cryarchiver recordings/ --merge-audio         # game and microphone tracks mixed into one
cryarchiver film.mkv --burn-subs=eng          # draw the English subtitles into the picture
cryarchiver film.mkv --strip -f webm          # only video and audio, as WebM
cryarchiver film.mkv -o clip.opus             # the audio of a video, as Opus
cryarchiver clip.mp4 --max-bitrate 12.5       # for sharing: at most 12.5 Mbit/s
cryarchiver clip.mp4 -o clip.avif             # a video as a looping animated AVIF
cryarchiver film.mkv --resize 1280:-1         # 1280 pixels wide, the aspect ratio kept
```

- Sources are never modified.
- Video is deinterlaced (or inverse-telecined), rotated upright, and keeps its colour, HDR10/HLG, every audio track, subtitles, chapters and fonts.
  - Personal metadata (GPS, camera and dates) and telemetry streams are dropped unless you pass `--keep-metadata`.
  - Dolby Vision is saved as its HDR10- or HLG-compatible base layer. Profile 5, which has none, keeps the original.
- At `--speed moderate` (the default) and `slow`, each video's quality setting is chosen before the full encode: a few seconds of samples are encoded and measured with VMAF, and the finished video is measured again over its whole length (a sample of frames spread over every second) before it is kept. What each kind of video needed is remembered, so at moderate similar videos skip the quality search, though a few samples still check that the output will be small enough; a video of 10 minutes or more, or one whose full encode is expected to take 15 minutes or more, is searched again, starting from what was remembered. `--speed fast` uses fixed settings and doesn't measure video.
  - A plain run remembers this only for its own files; an archive project remembers it across runs inside the project file.
  - When no setting reaches the quality target, or the samples show the output wouldn't be meaningfully smaller, the original is kept before the full encode. A finished video that already isn't small enough keeps the original right away. One that misses the target is encoded once more with more quality, at the setting the search's samples point to once moved by what the full check measured (the progress line says "encoding again"); if that encode isn't small enough or misses too, the original is kept, and a second encode that the samples already show would come out too large isn't made at all. A requested output (`--ss`/`--to`, `-f`, `--strip`, `--burn-subs`, `--merge-audio`, `--resize` or `--max-bitrate`) is written anyway, with a warning that says when the trim alone wrote it: when no setting reaches the target, it is encoded at the closest one without a full check.
  - When a full check, the first or the last, misses the target by at most 0.5 VMAF, on the mean and on the floor alike, and the output is still small enough to keep, it is written as it is, with a warning, instead of being encoded again.
  - Each full check of a whole video at its own size that passes or nearly passes also teaches its kind of video how far the samples' score was from the full score, and later searches aim that much higher (at most 3 VMAF points).
  - The result line of an encoded video ends with the CRF it was encoded at and, when the full check ran, its measured VMAF.
  - If ffmpeg has no libvmaf, moderate and slow don't measure video: each video is encoded at the preset's setting (or one an archive project learned earlier), still kept as the original when samples show it wouldn't be meaningfully smaller, and gets a warning that its quality wasn't measured.
  - Noisy sources at moderate and slow are denoised and get AV1 film grain back, with a strength that follows the measured noise. Transparent videos and videos too large for SVT-AV1 use other encoders and get no film grain.
- Images are rotated upright and keep their colour profile. Each encode is checked with SSIMULACRA2 and kept only if it reaches the quality target for the chosen speed.
  - A photo that misses its quality target is encoded again with more quality, at most twice; photos of the same kind then start from what worked.
  - Personal metadata (EXIF, XMP, GPS, IPTC) is dropped unless you pass `--keep-metadata`, which carries it over with the orientation and size corrected.
  - Photos with HDR gain maps or depth maps keep the original; `--drop-aux` converts them without the extra images.
  - RAW camera files, layered files (PSD, XCF), CMYK JPEGs and multi-page TIFFs are never converted.
- One video is encoded at a time while audio and images run in parallel; `-j N` allows N videos at once.
- Damaged or truncated videos fail with "source damaged" instead of producing a broken copy.
- An existing output is skipped, so running the same command again is safe.
- Files that would share an output name (song.flac and song.wav) are not merged: the first in name order is compressed, the second is skipped with a note.
- Cover art and common music tags are kept.
- `--keep-metadata` also keeps each file's timestamps.

**Custom quality.** Three flags replace a speed preset's quality targets and keep the rest of the preset. The run then names the preset as custom, for example `MODERATE · custom: video VMAF 90/85, image 80`.
- `--video-vmaf MEAN/FLOOR` is the VMAF mean and 5th-percentile floor each video must reach: mean 80–99, floor from 70 up to the mean. The presets use 93/89 at moderate and 95/91 at slow. Noisy sources encoded with film grain still get 2 off the mean and 2.5 off the floor. `fast` never measures video quality, so it refuses this flag.
- `--image-target N` is the SSIMULACRA2 score each image must reach, 50–95. The presets use 80, 85 and 90 at fast, moderate and slow.
- `--audio-kbps N` is the stereo Opus bitrate, 64–320 in steps of 8. Other channel layouts scale with it (mono gets half), and it applies to the audio tracks inside videos too. A lossy source with a lower bitrate is encoded at its own bitrate instead, raised to the fast preset's rate for its layout (96 kbps for stereo) but never above N.
- Video and image values are rounded to 0.1. What is learned about each kind of file is kept per target, so a custom run never reuses what a preset run learned.

| Option | Meaning | Default |
|---|---|---|
| `-o, --output PATH` | Output folder, or a file for a single input (its extension picks the format) | next to each source |
| `-f, --format mkv\|webm\|mp4\|opus\|ogg\|mka\|avif\|jxl` | Output format for every file; a requested output is written even when it isn't smaller | automatic |
| `-c, --codec latest` | Codec family (`compat` arrives in a later version) | latest |
| `-s, --speed slow\|moderate\|fast` | Trade speed for quality | moderate |
| `--video-vmaf MEAN/FLOOR` | Video quality target (moderate and slow) | the preset's |
| `--image-target N` | Image quality target (SSIMULACRA2) | the preset's |
| `--audio-kbps N` | Stereo audio bitrate; other layouts scale with it | the preset's |
| `--keep-metadata` | Keep personal metadata and file timestamps | off |
| `--drop-aux` | Convert photos that have gain or depth maps, dropping the extras | off |
| `--ss TIME`, `--to TIME` | Cut audio or video: `90`, `1:30` or `00:01:30.500` | whole file |
| `--merge-audio` | Mix all audio tracks into one | off |
| `--burn-subs[=TRACK\|LANG]` | Draw subtitles into the picture | off |
| `--sub-size PERCENT` | Height of burned text subtitles | 5 |
| `--strip` | Keep only the main video and the audio | off |
| `--resize W:H` | Scale video, animations and images; `W:-1` or `-1:H` keeps the aspect ratio. A resized output is always written | off |
| `--max-bitrate MBPS` | Cap the video bitrate for sharing, 0.1 to 1000, e.g. `12.5`; a capped output is always written | none |
| `--overwrite` | Replace existing outputs (never sources) | off |
| `--min-savings PERCENT` | Keep the original unless the output is at least this much smaller (0–90) | 10 |
| `-j, --jobs N` | Files processed in parallel (also videos, which otherwise run one at a time) | half the CPU cores, at most 4 |
| `--threads N` | Threads per file | automatic |
| `--priority normal\|low\|idle` | Process priority; `low` is below normal | normal |
| `--cpu-limit PERCENT` | Use at most this share of the CPU | none |
| `--ffmpeg PATH` | ffmpeg binary or its folder | from PATH |
| `-q, --quiet` | Only print failures and the summary | off |
| `-v, --verbose` | Show warnings as they happen and 20 lines of a failed tool's output | off |

Hidden advanced options (`--help` lists them): `--crf N` encodes video at a fixed CRF without the quality search (the quality isn't measured, and it can't be combined with `--video-vmaf`), `--video-encoder svt-av1|aom`, `--image-encoder avif|jxl|jxl-lossless`.

A plain run writes nothing but its outputs: no settings file, no cache and no log files. Errors are printed to the console; redirect the output to keep it. Locked files, files still being written, crashed or stalled tools and a full disk are retried automatically, up to three attempts; a crashed or out-of-memory tool is retried with half the threads it ran on, in plain runs and projects alike. A quality miss is not an error and is never retried.

`-o` with an audio extension (`.opus`, `.ogg`, `.mka`) or `-f opus|ogg|mka` on a video writes only its audio. `-o` with `.avif` or `-f avif` on a video writes an animated AVIF that loops forever, with no sound and no subtitles (`--burn-subs` draws one into the picture); the free disk space for its frames is checked first, and `--crf`, `--video-encoder`, `--video-vmaf`, `--merge-audio` and `--strip` don't apply to it. Both are for plain runs only.

`--max-bitrate` applies to video only: audio and images behave as without it, and an AVIF or JXL output (`-f` or `-o`) with it is refused. A video already under the cap is still re-encoded. It applies to plain runs only; archive projects don't take it. When the cap keeps a video from its quality target, the video is still written, with a warning.

`--resize` takes whole pixels, at most 65536 a side; leave one side out (`1280:-1`, `-1:720` or `1280:`) to keep the aspect ratio. A video written as a video file needs even sides. Audio ignores it.

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
- The project file stays small: its records are compressed, failure logs keep the last 100 lines of the last 5 attempts per file (32 MB of logs at most in all, dropping the logs of files that no longer fail first), and once it has grown, the file is compacted after a finished run; after a crash, it is compacted when it is next opened. A project file in an older format is upgraded the first time it opens; the upgrade writes a new file next to it and replaces the old one only once it is complete.
- A file's details in the dashboard (Enter on a file) show how it was encoded: the CRF and how it was chosen, the quality scores, a second encode, film grain, the sizes and any warnings. Only the latest job's record of a file is kept.
- With watch mode on, the run keeps going once everything is archived and picks up new files as they appear.
- The performance limit runs in the background: low priority, one job at a time, and at most the chosen share of the CPU.
- `project create` takes the custom quality flags (`--video-vmaf`, `--image-target`, `--audio-kbps`) and stores them in the project. Like a speed change, a change to them applies to files archived from then on; files already archived are not redone. `run` names the custom profile in its summary, and `project dump` shows the stored values. The dashboard's settings (`s`) change them too, and reset them to the preset.

**Deleting originals** is off by default. It can only be turned on by typing `DELETE`: in the dashboard's settings (`s`) or in the start menu's new-project form. An original is then deleted permanently, with no Recycle Bin, once its archived copy has been read back from disk and matches its BLAKE3. A headless run of such a project also needs `--delete-originals`, or it refuses to start. When the dashboard opens such a project, it archives as usual but deletes nothing until you type `DELETE` again in its settings, once per session.

An original is kept, not deleted, when its archived copy is the original itself or a hard link to it, is a link, has a second name (a hard link, for example into a backup tree), or is a reparse point such as a OneDrive cloud placeholder.

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

`cryarchiver` with no arguments, or `cryarchiver tui`, opens the start menu: the last 10 projects you opened, and a form that creates a new project and checks every field as you type. `cryarchiver --demo` or `cryarchiver tui --demo` shows the dashboard on demo data instead. The list of recent projects is kept in `recent.bin` next to the executable when that folder is writable; otherwise the start menu has no recent list. It only changes when you open or create a project, or remove projects from it in the menu.

The dashboard needs a terminal of at least 100×30; press `?` for keys and `q` to quit. Quitting stops running jobs, and the project resumes them on the next open.
