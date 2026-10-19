# Folde

A minimal Rust desktop application with GPUI and GPUI Kit. Two native pickers
are stacked vertically: the first selects a source directory, and the second
selects a destination directory. Each selected path is shown beneath its button.
Canceling a picker keeps the previous selection. **Start transfer** becomes
enabled once both directories are selected and their paths differ. Clicking it
recursively moves the source files and opens the transfer screen. The screen
shows the total file count, moved count, current file, completion, and any issues.
**Back** becomes available when the transfer finishes and keeps the selections.

## File organization

Files are grouped by creation date under the destination:

```text
destination/
  2024/
    April/
      07/
        photo.jpg
        photo-1.jpg
        photo-2.jpg
```

The transfer reads EXIF `DateTimeOriginal` first, then `DateTimeDigitized`. It
keeps the date recorded by the camera. If neither contains a valid date, it uses
filesystem creation time in the computer's local timezone. If creation time is
unavailable too, the file stays in the source and the issue is reported.

Month names are English, and days have two digits. When a filename collides, the
transfer compares SHA-256 hashes. If the contents match, it removes the source
file and leaves the existing destination file in place. Different contents
receive the first available `-N` suffix before the extension, checking each
colliding name for an identical file; existing files are never overwritten.
Files from subdirectories are combined into the dated folders.
Empty source directories remain in place. Symbolic links are skipped, and a
destination inside the source is excluded from the scan.

Transfers run in the background. Moves across drives copy to a temporary file,
publish it without overwriting existing files, and remove the source only after
the complete copy succeeds. Individual failures are reported and other files
continue. Full issue messages are also printed in the terminal.

`gpui-kit` re-exports its matching GPUI version. Its default features include
styled components and icon assets.

## Run

```sh
cargo run --locked
```

The first build can take several minutes.

On Windows, use the MSVC Rust toolchain and install Visual Studio 2022's
**Desktop development with C++** workload, a Windows SDK, and CMake. If Visual
Studio installed CMake but it is missing from `PATH`, run the following in
PowerShell before running Cargo:

```powershell
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$cmake = & $vswhere -latest -products '*' -find 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe'
$env:PATH = "$(Split-Path -Parent $cmake);$env:PATH"
cargo run --locked
```

See the official [installation guide](https://gpui-kit.com/docs/installation/)
for toolchain requirements on each platform and the
[getting started guide](https://gpui-kit.com/docs/getting-started/) for GPUI's
view and application lifecycle.

## Development

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
```

The app initializes GPUI Kit before opening a window, registers its bundled
assets, and uses `cx.prompt_for_paths` to open native file and directory dialogs.
Selections are retained in the view and updated using `cx.spawn`, `cx.listener`,
and `cx.notify`.
