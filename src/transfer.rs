use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, FileTimes},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime};
use exif::{In, Reader, Tag, Value};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

pub enum TransferEvent {
    Scanned(usize),
    Moving(PathBuf),
    Moved,
    Error(String),
    Finished,
}

/// Runs on a background thread. Returning false from `emit` stops further work.
pub fn run(source: &Path, destination: &Path, mut emit: impl FnMut(TransferEvent) -> bool) {
    if let Err(error) = transfer_files(source, destination, &mut emit) {
        emit(TransferEvent::Error(error.to_string()));
    }
    emit(TransferEvent::Finished);
}

fn transfer_files(
    source: &Path,
    destination: &Path,
    emit: &mut impl FnMut(TransferEvent) -> bool,
) -> io::Result<()> {
    let source = source.canonicalize()?;
    let destination = destination.canonicalize()?;
    if !source.is_dir() || !destination.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source and destination must both be directories",
        ));
    }
    if source == destination {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source and destination resolve to the same directory",
        ));
    }

    // Snapshot before moving anything, and prune a destination inside the source.
    let exclude_destination = destination.starts_with(&source);
    let entries = WalkDir::new(&source)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| !exclude_destination || !entry.path().starts_with(&destination));
    let mut files = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_type().is_file() => files.push(entry.into_path()),
            Ok(_) => {}
            Err(error) => {
                if !emit(TransferEvent::Error(format!("Could not scan: {error}"))) {
                    return Ok(());
                }
            }
        }
    }
    if !emit(TransferEvent::Scanned(files.len())) {
        return Ok(());
    }

    for file in files {
        if !emit(TransferEvent::Moving(file.clone())) {
            break;
        }
        let result = move_file(&file, &source, &destination);
        let event = match result {
            Ok(_) => TransferEvent::Moved,
            Err(error) => TransferEvent::Error(format!("{}: {error}", file.display())),
        };
        if !emit(event) {
            break;
        }
    }
    Ok(())
}

fn move_file(file: &Path, source: &Path, destination: &Path) -> io::Result<PathBuf> {
    // Recheck after scanning so a replaced file/link cannot escape the source.
    if !fs::symlink_metadata(file)?.file_type().is_file()
        || !file.canonicalize()?.starts_with(source)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Source entry is no longer a regular file inside the source directory",
        ));
    }
    let date = creation_date(file)?;
    let directory = dated_directory(destination, date)?;

    let filename = file
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Source has no file name"))?;
    for suffix in 0..=u64::MAX {
        let target = directory.join(suffixed_name(filename, suffix));
        if occupied(&target)? {
            if remove_identical_source(file, &target)? {
                return Ok(target);
            }
            continue;
        }
        // A new hard link plus unlink preserves metadata and avoids copying on
        // the same filesystem. Unlike rename, hard_link never replaces a file.
        match fs::hard_link(file, &target) {
            Ok(()) => {
                remove_source_or_rollback(file, &target)?;
                return Ok(target);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if remove_identical_source(file, &target)? {
                    return Ok(target);
                }
            }
            // Other volumes and filesystems without hard links use copy/delete.
            Err(_) => return copy_and_remove(file, &directory, filename, suffix),
        }
    }
    Err(io::Error::other("No available file name"))
}

fn dated_directory(destination: &Path, date: NaiveDate) -> io::Result<PathBuf> {
    let mut directory = destination.to_owned();
    for component in ["%Y", "%B", "%d"] {
        directory.push(date.format(component).to_string());
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Check each level before descending through any pre-existing links.
        directory = directory.canonicalize()?;
        if !directory.starts_with(destination) || !directory.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Date directory is not a directory inside the destination",
            ));
        }
    }
    Ok(directory)
}

fn creation_date(path: &Path) -> io::Result<NaiveDate> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let mut reader = BufReader::new(file);
    let exif = Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut reader)
        .or_else(|error| error.distill_partial_result(|_| {}));
    if let Ok(exif) = exif {
        // Keep the camera's recorded calendar date, regardless of UTC offset.
        for tag in [Tag::DateTimeOriginal, Tag::DateTimeDigitized] {
            if let Some(field) = exif.get_field(tag, In::PRIMARY)
                && let Value::Ascii(values) = &field.value
                && let Some(value) = values.first()
                && let Ok(value) = std::str::from_utf8(value)
                && let Ok(date) = NaiveDateTime::parse_from_str(
                    value.trim_matches(|c: char| c == '\0' || c.is_ascii_whitespace()),
                    "%Y:%m:%d %H:%M:%S",
                )
                && (1..=9999).contains(&date.year())
            {
                return Ok(date.date());
            }
        }
    }
    let created = metadata.created().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("No valid EXIF date and filesystem creation time is unavailable: {error}"),
        )
    })?;
    Ok(DateTime::<Local>::from(created).date_naive())
}

fn suffixed_name(filename: &OsStr, suffix: u64) -> OsString {
    if suffix == 0 {
        return filename.to_owned();
    }
    let path = Path::new(filename);
    let mut name = path.file_stem().unwrap_or(filename).to_owned();
    name.push(format!("-{suffix}"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    name
}

fn occupied(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn remove_identical_source(source: &Path, target: &Path) -> io::Result<bool> {
    let target_metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        // A colliding entry may have disappeared since the publishing attempt.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // Directories and links must not cause the source to be discarded.
    if !target_metadata.file_type().is_file()
        || fs::metadata(source)?.len() != target_metadata.len()
        || file_hash(source)? != file_hash(target)?
    {
        return Ok(false);
    }
    // This target already existed: never roll it back if source removal fails.
    fs::remove_file(source)?;
    Ok(true)
}

fn file_hash(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().into())
}

fn copy_and_remove(
    source: &Path,
    directory: &Path,
    filename: &OsStr,
    initial_suffix: u64,
) -> io::Result<PathBuf> {
    let mut input = File::open(source)?;
    let metadata = input.metadata()?;
    let mut temporary = NamedTempFile::new_in(directory)?;
    let copied = io::copy(&mut input, temporary.as_file_mut())?;
    if copied != metadata.len() || input.metadata()?.len() != metadata.len() {
        return Err(io::Error::other("Source size changed during the copy"));
    }
    let mut times = FileTimes::new();
    if let Ok(accessed) = metadata.accessed() {
        times = times.set_accessed(accessed);
    }
    if let Ok(modified) = metadata.modified() {
        times = times.set_modified(modified);
    }
    #[cfg(windows)]
    if let Ok(created) = metadata.created() {
        use std::os::windows::fs::FileTimesExt;
        times = times.set_created(created);
    }
    temporary.as_file().set_times(times)?;
    temporary
        .as_file()
        .set_permissions(metadata.permissions())?;
    temporary.as_file().sync_all()?;
    drop(input);

    for suffix in initial_suffix..=u64::MAX {
        let target = directory.join(suffixed_name(filename, suffix));
        if occupied(&target)? {
            if remove_identical_source(source, &target)? {
                return Ok(target);
            }
            continue;
        }
        // Publish a complete file atomically without replacing any existing one.
        match temporary.persist_noclobber(&target) {
            Ok(output) => {
                drop(output);
                remove_source_or_rollback(source, &target)?;
                return Ok(target);
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                temporary = error.file;
                if remove_identical_source(source, &target)? {
                    return Ok(target);
                }
            }
            Err(error) => return Err(error.error),
        }
    }
    Err(io::Error::other("No available file name"))
}

fn remove_source_or_rollback(source: &Path, destination: &Path) -> io::Result<()> {
    if let Err(error) = fs::remove_file(source) {
        return match fs::remove_file(destination) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(io::Error::new(
                error.kind(),
                format!(
                    "Could not remove source: {error}. A destination copy remains at {}: {cleanup}",
                    destination.display()
                ),
            )),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, io::Cursor, thread};

    use exif::{Field, experimental::Writer};
    use tempfile::{TempDir, tempdir};

    fn directories() -> (TempDir, PathBuf, PathBuf) {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        (root, source, destination)
    }

    fn jpeg(dates: &[(Tag, &str)]) -> Vec<u8> {
        let fields: Vec<_> = dates
            .iter()
            .map(|(tag, date)| Field {
                tag: *tag,
                ifd_num: In::PRIMARY,
                value: Value::Ascii(vec![date.as_bytes().to_vec()]),
            })
            .collect();
        let mut writer = Writer::new();
        for field in &fields {
            writer.push_field(field);
        }
        let mut tiff = Cursor::new(Vec::new());
        writer.write(&mut tiff, true).unwrap();
        let tiff = tiff.into_inner();
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
        jpeg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        jpeg.extend_from_slice(b"Exif\0\0");
        jpeg.extend_from_slice(&tiff);
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        jpeg
    }

    fn capture(date: &str) -> Vec<u8> {
        jpeg(&[(Tag::DateTimeOriginal, date)])
    }

    fn events(source: &Path, destination: &Path) -> Vec<TransferEvent> {
        let mut events = Vec::new();
        run(source, destination, |event| {
            events.push(event);
            true
        });
        events
    }

    fn counts(events: &[TransferEvent]) -> (usize, usize, usize) {
        let total = events
            .iter()
            .find_map(|event| match event {
                TransferEvent::Scanned(total) => Some(*total),
                _ => None,
            })
            .unwrap_or(0);
        let moved = events
            .iter()
            .filter(|event| matches!(event, TransferEvent::Moved))
            .count();
        let errors = events
            .iter()
            .filter(|event| matches!(event, TransferEvent::Error(_)))
            .count();
        assert!(matches!(events.last(), Some(TransferEvent::Finished)));
        (total, moved, errors)
    }

    #[test]
    fn recursively_uses_capture_dates_and_numbers_duplicate_names() {
        let (_root, source, destination) = directories();
        fs::create_dir_all(source.join("a/nested")).unwrap();
        fs::create_dir_all(source.join("b")).unwrap();
        let first = jpeg(&[
            (Tag::DateTimeOriginal, "2024:04:07 23:59:59"),
            (Tag::DateTimeDigitized, "2025:01:01 12:00:00"),
        ]);
        let second = capture("2024:04:07 00:00:01");
        fs::write(source.join("a/nested/photo.jpg"), &first).unwrap();
        fs::write(source.join("b/photo.jpg"), &second).unwrap();
        let day = destination.join("2024/April/07");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("photo.jpg"), b"existing").unwrap();

        assert_eq!(counts(&events(&source, &destination)), (2, 2, 0));
        assert_eq!(fs::read(day.join("photo.jpg")).unwrap(), b"existing");
        assert_eq!(fs::read(day.join("photo-1.jpg")).unwrap(), first);
        assert_eq!(fs::read(day.join("photo-2.jpg")).unwrap(), second);
        assert!(!source.join("a/nested/photo.jpg").exists());
        assert!(!source.join("b/photo.jpg").exists());
    }

    #[test]
    fn identical_collision_removes_source_and_preserves_existing_target() {
        let (_root, source, destination) = directories();
        let file = source.join("photo.jpg");
        let bytes = capture("2024:04:07 12:00:00");
        fs::write(&file, &bytes).unwrap();
        let day = destination.join("2024/April/07");
        fs::create_dir_all(&day).unwrap();
        let target = day.join("photo.jpg");
        fs::write(&target, &bytes).unwrap();
        File::options()
            .write(true)
            .open(&target)
            .unwrap()
            .set_times(FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let before = fs::metadata(&target).unwrap();

        assert_eq!(counts(&events(&source, &destination)), (1, 1, 0));
        assert!(!file.exists());
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::read_dir(&day).unwrap().count(), 1);
        let after = fs::metadata(&target).unwrap();
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert_eq!(after.created().unwrap(), before.created().unwrap());
        assert_eq!(after.permissions(), before.permissions());
    }

    #[test]
    fn identical_collision_at_numbered_name_removes_source() {
        let (_root, source, destination) = directories();
        let bytes = capture("2024:04:07 12:00:00");
        let different = capture("2024:04:07 12:00:01");
        assert_eq!(bytes.len(), different.len());
        fs::write(source.join("photo.jpg"), &bytes).unwrap();
        let day = destination.join("2024/April/07");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("photo.jpg"), &different).unwrap();
        fs::write(day.join("photo-1.jpg"), &bytes).unwrap();

        assert_eq!(counts(&events(&source, &destination)), (1, 1, 0));
        assert!(!source.join("photo.jpg").exists());
        assert_eq!(fs::read(day.join("photo.jpg")).unwrap(), different);
        assert_eq!(fs::read(day.join("photo-1.jpg")).unwrap(), bytes);
        assert_eq!(fs::read_dir(day).unwrap().count(), 2);
    }

    #[test]
    fn identical_files_from_source_subdirectories_share_one_target() {
        let (_root, source, destination) = directories();
        let bytes = capture("2024:04:07 12:00:00");
        for directory in ["a", "b"] {
            fs::create_dir(source.join(directory)).unwrap();
            fs::write(source.join(directory).join("photo.jpg"), &bytes).unwrap();
        }

        assert_eq!(counts(&events(&source, &destination)), (2, 2, 0));
        assert!(!source.join("a/photo.jpg").exists());
        assert!(!source.join("b/photo.jpg").exists());
        let day = destination.join("2024/April/07");
        assert_eq!(fs::read(day.join("photo.jpg")).unwrap(), bytes);
        assert_eq!(fs::read_dir(day).unwrap().count(), 1);
    }

    #[test]
    fn falls_back_from_invalid_original_to_digitized_date() {
        let (_root, source, destination) = directories();
        fs::write(
            source.join("photo.jpg"),
            jpeg(&[
                (Tag::DateTimeOriginal, "0000:00:00 00:00:00"),
                (Tag::DateTimeDigitized, "2024:02:29 12:00:00"),
            ]),
        )
        .unwrap();
        assert_eq!(counts(&events(&source, &destination)), (1, 1, 0));
        assert!(destination.join("2024/February/29/photo.jpg").is_file());
    }

    #[test]
    fn falls_back_to_filesystem_creation_time_for_non_exif_and_invalid_exif() {
        let (_root, source, destination) = directories();
        let inputs = [
            ("notes.txt", b"plain text".to_vec()),
            ("invalid.jpg", capture("2024:02:30 12:00:00")),
        ];
        let mut expected = Vec::new();
        for (name, bytes) in inputs {
            let file = source.join(name);
            fs::write(&file, &bytes).unwrap();
            let date = DateTime::<Local>::from(fs::metadata(&file).unwrap().created().unwrap());
            expected.push((
                destination
                    .join(date.format("%Y/%B/%d").to_string())
                    .join(name),
                bytes,
            ));
        }
        assert_eq!(counts(&events(&source, &destination)), (2, 2, 0));
        for (path, bytes) in expected {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn excludes_destination_inside_source() {
        let (_root, source, _) = directories();
        let destination = source.join("archive");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("existing.txt"), b"existing").unwrap();
        fs::write(source.join("photo.jpg"), capture("2024:04:07 12:00:00")).unwrap();

        assert_eq!(counts(&events(&source, &destination)), (1, 1, 0));
        assert_eq!(
            fs::read(destination.join("existing.txt")).unwrap(),
            b"existing"
        );
        assert!(destination.join("2024/April/07/photo.jpg").is_file());
    }

    #[test]
    fn rejects_source_and_destination_aliases_without_moving_anything() {
        let (_root, source, _) = directories();
        fs::write(source.join("photo.jpg"), b"original").unwrap();
        assert_eq!(counts(&events(&source, &source.join("."))), (0, 0, 1));
        assert_eq!(fs::read(source.join("photo.jpg")).unwrap(), b"original");
    }

    #[test]
    fn keeps_failed_files_and_continues_with_other_files() {
        let (_root, source, destination) = directories();
        let blocked = capture("2024:04:07 12:00:00");
        fs::write(source.join("blocked.jpg"), &blocked).unwrap();
        fs::write(source.join("good.jpg"), capture("2025:05:08 12:00:00")).unwrap();
        fs::write(destination.join("2024"), b"directory blocked").unwrap();

        assert_eq!(counts(&events(&source, &destination)), (2, 1, 1));
        assert_eq!(fs::read(source.join("blocked.jpg")).unwrap(), blocked);
        assert_eq!(
            fs::read(destination.join("2024")).unwrap(),
            b"directory blocked"
        );
        assert!(!source.join("good.jpg").exists());
        assert!(destination.join("2025/May/08/good.jpg").is_file());
    }

    #[test]
    fn copy_fallback_preserves_contents_times_and_existing_files() {
        let (_root, source, destination) = directories();
        let file = source.join("photo.jpg");
        fs::write(&file, b"copied contents").unwrap();
        let before = fs::metadata(&file).unwrap();
        fs::write(destination.join("photo.jpg"), b"existing").unwrap();
        fs::create_dir(destination.join("photo-1.jpg")).unwrap();

        let target = copy_and_remove(&file, &destination, OsStr::new("photo.jpg"), 0).unwrap();
        assert_eq!(target, destination.join("photo-2.jpg"));
        assert!(!file.exists());
        assert_eq!(fs::read(&target).unwrap(), b"copied contents");
        assert_eq!(
            fs::read(destination.join("photo.jpg")).unwrap(),
            b"existing"
        );
        let after = fs::metadata(&target).unwrap();
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        #[cfg(windows)]
        assert_eq!(after.created().unwrap(), before.created().unwrap());
    }

    #[test]
    fn copy_fallback_discards_duplicates_including_empty_and_large_files() {
        for bytes in [Vec::new(), vec![7; 2 * 64 * 1024 + 1]] {
            for suffix in [0, 1] {
                let (_root, source, destination) = directories();
                let filename = OsStr::new("photo.jpg");
                let file = source.join(filename);
                fs::write(&file, &bytes).unwrap();
                // A same-size difference beyond the first hash buffer must not match.
                let mut different = bytes.clone();
                if let Some(last) = different.last_mut() {
                    *last = 8;
                } else {
                    different.push(8);
                }
                if suffix == 1 {
                    fs::write(destination.join(filename), &different).unwrap();
                }
                let target = destination.join(suffixed_name(filename, suffix));
                fs::write(&target, &bytes).unwrap();
                let before = fs::metadata(&target).unwrap();

                assert_eq!(
                    copy_and_remove(&file, &destination, filename, 0).unwrap(),
                    target
                );
                assert!(!file.exists());
                assert_eq!(fs::read(&target).unwrap(), bytes);
                let after = fs::metadata(&target).unwrap();
                assert_eq!(after.modified().unwrap(), before.modified().unwrap());
                assert_eq!(after.created().unwrap(), before.created().unwrap());
                assert_eq!(
                    fs::read_dir(&destination).unwrap().count(),
                    suffix as usize + 1
                );
                if suffix == 1 {
                    assert_eq!(fs::read(destination.join(filename)).unwrap(), different);
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn failed_duplicate_removal_preserves_existing_target() {
        use std::os::windows::fs::OpenOptionsExt;

        let (_root, source, destination) = directories();
        let file = source.join("photo.jpg");
        let bytes = capture("2024:04:07 12:00:00");
        fs::write(&file, &bytes).unwrap();
        let day = destination.join("2024/April/07");
        fs::create_dir_all(&day).unwrap();
        let target = day.join("photo.jpg");
        fs::write(&target, &bytes).unwrap();
        let locked = File::options()
            .read(true)
            // Allow hashing, but deny deletion while this handle remains open.
            .share_mode(0x1 | 0x2) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .open(&file)
            .unwrap();

        let result = counts(&events(&source, &destination));
        drop(locked);
        assert_eq!(result, (1, 0, 1));
        assert_eq!(fs::read(&file).unwrap(), bytes);
        assert_eq!(fs::read(&target).unwrap(), bytes);
        assert_eq!(fs::read_dir(day).unwrap().count(), 1);
    }

    #[test]
    fn copy_failure_keeps_source_intact() {
        let (_root, source, destination) = directories();
        let file = source.join("photo.jpg");
        fs::write(&file, b"original").unwrap();
        let invalid_directory = destination.join("blocked");
        fs::write(&invalid_directory, b"existing").unwrap();

        assert!(copy_and_remove(&file, &invalid_directory, OsStr::new("photo.jpg"), 0).is_err());
        assert_eq!(fs::read(file).unwrap(), b"original");
        assert_eq!(fs::read(invalid_directory).unwrap(), b"existing");
    }

    #[test]
    fn concurrent_transfers_do_not_overwrite_colliding_files() {
        let (root, _, destination) = directories();
        let day = destination.join("2024/April/07");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("photo.jpg"), b"existing").unwrap();
        let mut expected = HashSet::from([b"existing".to_vec()]);
        let mut workers = Vec::new();
        for index in 0..8 {
            let source = root.path().join(format!("source-{index}"));
            fs::create_dir_all(&source).unwrap();
            let bytes = capture(&format!("2024:04:07 12:00:{index:02}"));
            expected.insert(bytes.clone());
            fs::write(source.join("photo.jpg"), bytes).unwrap();
            let destination = destination.clone();
            workers.push(thread::spawn(move || {
                assert_eq!(counts(&events(&source, &destination)), (1, 1, 0));
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let actual: HashSet<_> = fs::read_dir(day)
            .unwrap()
            .map(|entry| fs::read(entry.unwrap().path()).unwrap())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn cancellation_before_moving_preserves_all_files() {
        let (_root, source, destination) = directories();
        fs::write(source.join("photo.jpg"), capture("2024:04:07 12:00:00")).unwrap();
        run(&source, &destination, |event| {
            !matches!(event, TransferEvent::Scanned(_))
        });
        assert!(source.join("photo.jpg").is_file());
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_date_directory_links_that_escape_destination() {
        let (root, source, destination) = directories();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, destination.join("2024")).unwrap();
        fs::write(source.join("photo.jpg"), capture("2024:04:07 12:00:00")).unwrap();

        assert_eq!(counts(&events(&source, &destination)), (1, 0, 1));
        assert!(source.join("photo.jpg").is_file());
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }
}
