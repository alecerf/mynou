## Fixed: A damaged extra no longer fails an import

When a download held several media files, a single one that could not be read,
such as a truncated sample, failed the whole import. Such files are now skipped;
the import fails only if no file with video remains, and its message names the
file that could not be read. Files are compared by size first, so a film is
analyzed alone instead of with every extra, and a season pack analyzes only the
file named for the episode.

## Fixed: Long MP4 films import again

An MP4 whose frames have uneven durations, as after a remux from Matroska or
from a phone at a variable frame rate, was refused once it held more than
100,000 timing entries, about two hours of film: its import failed with
"Invalid or oversized MP4 timing table". Such files are now read.

## Fixed: AVI duration and audio, QuickTime language

An AVI larger than 1 GiB reported the duration of its first gigabyte only; the
total now comes from its OpenDML header. Multichannel audio stored as
`WAVE_FORMAT_EXTENSIBLE` is named by its codec, such as `ac3`, instead of
`wav_0xfffe`. QuickTime files that give their language as a Macintosh code now
report it, such as `eng`.

## Changed: Media files are analyzed in one read or a few

Analyzing a file no longer walks through its media data. A two-hour MKV went
from about 3,000 scattered reads (47 MB, half a second on an internal SSD,
seconds on a hard disk or network share) to one 16 KiB read; an AVI also needs
one read, and an MP4 a few, with no long table read. See the
[media benchmark](/docs/media-benchmark.md).
