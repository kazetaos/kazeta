use std::path::Path;
use std::path::PathBuf;
use std::fs;
use std::collections::VecDeque;
use std::io::{self, BufRead, Write, Read};
use sysinfo::Disks;
use tar::{Builder, Archive};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use walkdir;
use std::process::Command;
use chrono::DateTime;

// Directories to exclude from size calculation and copying
const EXCLUDED_DIRS: &[&str] = &[
    ".cache",
    ".config/pulse/cookie",
    ".kazeta/share",
    ".kazeta/var/prefix/dosdevices",
    ".kazeta/var/prefix/drive_c/windows",
    ".kazeta/var/prefix/pfx"
];

fn should_exclude_path(path: &Path) -> bool {
    let path_str = path.to_str().unwrap_or("");
    EXCLUDED_DIRS.iter().any(|&excluded| path_str.contains(excluded))
}

/// Searches for files with a given extension within a directory up to a specified depth
///
/// # Arguments
/// * `dir` - The directory to search in
/// * `extension` - The file extension to search for (without the dot, e.g., "txt", "rs")
/// * `max_depth` - Maximum depth to search (0 = only current directory)
/// * `find_first` - If true, stops after finding the first match
///
/// # Returns
/// * `Result<Vec<PathBuf>, io::Error>` - Vector of found file paths or an error
///
/// # Note
/// This function ignores permission errors and other I/O errors for individual files/directories
/// and continues searching. It only returns an error if the initial directory is inaccessible.
/// Searches breadth-first (higher level directories first).
pub fn find_files_by_extension<P: AsRef<Path>>(
    dir: P,
    extension: &str,
    max_depth: usize,
    find_first: bool,
) -> Result<Vec<PathBuf>, io::Error> {
    let dir_path = dir.as_ref();

    // Check if initial directory exists and is accessible
    if !dir_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Directory does not exist: {}", dir_path.display())
        ));
    }

    // Try to read the initial directory to ensure it's accessible
    fs::read_dir(dir_path)?;

    let mut results = Vec::new();
    search_breadth_first(dir_path, extension, max_depth, find_first, &mut results);
    Ok(results)
}

fn search_breadth_first(
    start_dir: &Path,
    extension: &str,
    max_depth: usize,
    find_first: bool,
    results: &mut Vec<PathBuf>,
) {
    let mut queue = VecDeque::new();
    queue.push_back((start_dir.to_path_buf(), 0));

    while let Some((current_dir, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }

        let entries = match fs::read_dir(&current_dir) {
            Ok(entries) => entries,
            Err(_) => continue, // Skip directories we can't read
        };

        let mut subdirs = Vec::new();

        // First, process all files in the current directory
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue, // Skip entries we can't read
            };

            let path = entry.path();

            let metadata = match path.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue, // Skip files/dirs we can't get metadata for
            };

            if metadata.is_file() {
                // Check if file has the desired extension
                if let Some(file_ext) = path.extension() {
                    if file_ext.to_string_lossy().eq_ignore_ascii_case(extension) {
                        results.push(path);
                        if find_first {
                            return; // Exit immediately if we only want the first match
                        }
                    }
                }
            } else if metadata.is_dir() && depth < max_depth {
                // Collect subdirectories to process later
                subdirs.push(path);
            }
        }

        // Then add subdirectories to the queue for next level processing
        for subdir in subdirs {
            queue.push_back((subdir, depth + 1));
        }
    }
}

pub fn get_base_dir_from_drive_name(drive_name: &str) -> String {
    if drive_name == "internal" || drive_name.is_empty() {
        dirs::home_dir().unwrap().join(".local/share/kazeta").to_string_lossy().to_string()
    } else {
        let base_ext = if Path::new("/media").read_dir().map(|mut d| d.next().is_none()).unwrap_or(true) {
            if Path::new(&format!("/run/media/{}", whoami::username())).exists() {
                format!("/run/media/{}", whoami::username())
            } else {
                "/run/media".to_string()
            }
        } else {
            "/media".to_string()
        };

        Path::new(&base_ext).join(drive_name).to_string_lossy().to_string()
    }
}

pub fn get_save_dir_from_drive_name(drive_name: &str) -> String {
    let base_dir = get_base_dir_from_drive_name(drive_name);
    if drive_name == "internal" || drive_name.is_empty() {
        let save_dir = Path::new(&base_dir).join("saves/default");
        if !save_dir.exists() {
            fs::create_dir_all(&save_dir).unwrap_or_else(|e| {
                eprintln!("Failed to create save directory: {}", e);
            });
        }
        save_dir.to_string_lossy().into_owned()
    } else {
        let save_dir = Path::new(&base_dir).join("kazeta/saves");
        if !save_dir.exists() {
            fs::create_dir_all(&save_dir).unwrap_or_else(|e| {
                eprintln!("Failed to create save directory: {}", e);
            });
        }
        save_dir.to_string_lossy().into_owned()
    }
}

pub fn get_cache_dir_from_drive_name(drive_name: &str) -> String {
    let base_dir = dirs::home_dir().unwrap().join(".local/share/kazeta");
    if drive_name == "internal" || drive_name.is_empty() {
        let cache_dir = base_dir.join("cache");
        if !cache_dir.exists() {
            fs::create_dir_all(&cache_dir).unwrap_or_else(|e| {
                eprintln!("Failed to create cache directory: {}", e);
            });
        }
        cache_dir.to_string_lossy().into_owned()
    } else {
        let base_ext = if Path::new("/media").read_dir().map(|mut d| d.next().is_none()).unwrap_or(true) {
            if Path::new(&format!("/run/media/{}", whoami::username())).exists() {
                format!("/run/media/{}", whoami::username())
            } else {
                "/run/media".to_string()
            }
        } else {
            "/media".to_string()
        };

        let cache_dir = Path::new(&base_ext).join(drive_name).join("kazeta/cache");
        if !cache_dir.exists() {
            fs::create_dir_all(&cache_dir).unwrap_or_else(|e| {
                eprintln!("Failed to create cache directory: {}", e);
            });
        }
        cache_dir.to_string_lossy().into_owned()
    }
}

fn get_attribute(info_file: &Path, attribute: &str) -> io::Result<String> {
    let file = fs::File::open(info_file)?;
    let reader = io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.starts_with(&format!("{}=", attribute)) {
            return Ok(line[attribute.len() + 1..].to_string());
        }
    }

    Ok(String::new())
}

pub fn list_devices() -> io::Result<Vec<(String, u32)>> {
    let mut devices = Vec::new();
    let disks = Disks::new_with_refreshed_list();

    // Add internal drive
    let base_dir = dirs::home_dir().unwrap().join(".local/share/kazeta");
    let base_dir_str = base_dir.to_str().unwrap();

    // Find the disk that contains our base directory
    let internal_disk = disks.iter()
        .find(|disk| {
            let mount_point = disk.mount_point().to_str().unwrap();
            base_dir_str.starts_with(mount_point)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Could not find internal disk"))?;

    let free_space = (internal_disk.available_space() / 1024 / 1024) as u32; // Convert to MB
    devices.push(("internal".to_string(), free_space));

    // Add external drives
    let base_ext = if Path::new("/media").read_dir().map(|mut d| d.next().is_none()).unwrap_or(true) {
        if Path::new(&format!("/run/media/{}", whoami::username())).exists() {
            format!("/run/media/{}", whoami::username())
        } else {
            "/run/media".to_string()
        }
    } else {
        "/media".to_string()
    };

    // Find all disks mounted under the external base directory
    for disk in disks.iter() {
        let mount_point = disk.mount_point().to_str().unwrap();
        if mount_point.starts_with(&base_ext) {
            let name = mount_point.split('/').last().unwrap().to_string();
            if name == "frzr_efi" {
                // ignore internal frzr partition
                continue;
            }
            let free_space = (disk.available_space() / 1024 / 1024) as u32; // Convert to MB
            devices.push((name, free_space));
        }
    }

    Ok(devices)
}

pub fn has_save_dir(drive_name: &str) -> bool {
    if drive_name == "internal" {
        return true;
    }

    if is_cart(drive_name) {
        return false;
    }

    let save_dir = get_save_dir_from_drive_name(drive_name);
    Path::new(&save_dir).exists()
}

pub fn is_cart(drive_name: &str) -> bool {
    if drive_name == "internal" {
        return false;
    }

    let mount_point: String = get_base_dir_from_drive_name(drive_name);

    if let Ok(files) = find_files_by_extension(&mount_point, "kzi", 1, true) {
        if files.len() > 0 {
            return true;
        }
    }

    if let Ok(files) = find_files_by_extension(&mount_point, "kzp", 1, true) {
        if files.len() > 0 {
            return true;
        }
    }

    false
}

pub fn is_cart_connected() -> bool {
    if let Ok(files) = find_files_by_extension("/run/media", "kzi", 2, true) {
        if files.len() > 0 {
            return true;
        }
    }

    if let Ok(files) = find_files_by_extension("/run/media", "kzp", 2, true) {
        if files.len() > 0 {
            return true;
        }
    }

    false
}

pub fn get_save_details(drive_name: &str) -> io::Result<Vec<(String, String, String)>> {
    let save_dir = get_save_dir_from_drive_name(drive_name);
    let cache_dir = get_cache_dir_from_drive_name(drive_name);
    eprintln!("Getting save details from directory: {}", save_dir);
    let mut details = Vec::new();

    for entry in fs::read_dir(save_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid filename"))?;

        // Remove .tar extension if present
        let cart_id = if file_name.ends_with(".tar") {
            &file_name[..file_name.len() - 4]
        } else {
            file_name
        };

        let metadata_path = Path::new(&cache_dir).join(cart_id).join("metadata.kzi");
        let name = get_attribute(&metadata_path, "Name").unwrap_or_else(|e| {
            eprintln!("Failed to read metadata for {}: {}", cart_id, e);
            String::new()
        });
        let icon = format!("{}/{}/icon.png", cache_dir, cart_id);

        details.push((cart_id.to_string(), name, icon));
    }

    // Sort details alphabetically by name, fallback to cart_id if name is empty
    details.sort_by(|a, b| {
        let name_a = if a.1.is_empty() { &a.0 } else { &a.1 };
        let name_b = if b.1.is_empty() { &b.0 } else { &b.1 };
        name_a.to_lowercase().cmp(&name_b.to_lowercase())
    });

    eprintln!("Found {} save details", details.len());
    Ok(details)
}

pub fn delete_save(cart_id: &str, from_drive: &str) -> Result<(), String> {
    let from_dir = get_save_dir_from_drive_name(from_drive);
    let from_cache = get_cache_dir_from_drive_name(from_drive);

    // Check if save exists
    let save_path = Path::new(&from_dir).join(cart_id);
    let save_path_tar = Path::new(&from_dir).join(format!("{}.tar", cart_id));
    if !save_path.exists() && !save_path_tar.exists() {
        return Err(format!("Save file for {} does not exist on '{}' drive", cart_id, from_drive));
    }

    // Delete save file
    if from_drive == "internal" {
        fs::remove_dir_all(save_path).map_err(|e| e.to_string())?;
    } else {
        fs::remove_file(save_path_tar).map_err(|e| e.to_string())?;
    }

    // Delete cache
    let cache_path = Path::new(&from_cache).join(cart_id);
    if cache_path.exists() {
        fs::remove_dir_all(cache_path).map_err(|e| e.to_string())?;
    }

    Ok(())
}

pub fn copy_save(cart_id: &str, from_drive: &str, to_drive: &str, progress: Arc<AtomicU16>) -> Result<(), String> {
    let from_dir = get_save_dir_from_drive_name(from_drive);
    let to_dir = get_save_dir_from_drive_name(to_drive);
    let from_cache = get_cache_dir_from_drive_name(from_drive);
    let to_cache = get_cache_dir_from_drive_name(to_drive);

    if from_drive == to_drive {
        return Err("Cannot copy to same location".to_string());
    }

    // Check if source save exists
    let from_path = Path::new(&from_dir).join(cart_id);
    let from_path_tar = Path::new(&from_dir).join(format!("{}.tar", cart_id));
    if !from_path.exists() && !from_path_tar.exists() {
        return Err(format!("Save file for {} does not exist on '{}' drive", cart_id, from_drive));
    }

    // Check if destination save already exists
    let to_path = Path::new(&to_dir).join(cart_id);
    let to_path_tar = Path::new(&to_dir).join(format!("{}.tar", cart_id));
    if to_path.exists() || to_path_tar.exists() {
        return Err(format!("Save file for {} already exists on '{}'", cart_id, to_drive));
    }

    // Create destination directories
    fs::create_dir_all(&to_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&to_cache).map_err(|e| e.to_string())?;

    // Copy save data
    let result = if from_drive == "internal" {
        // Internal to external: create tar archive
        eprintln!("Starting internal to external copy for {}", cart_id);
        let file = fs::File::create(&to_path_tar).map_err(|e| format!("Failed to create destination file: {}", e))?;
        let mut builder = Builder::new(file);

        // Calculate total size for progress reporting
        let mut total_size = 0;
        for entry in walkdir::WalkDir::new(&from_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                // Skip excluded directories and their contents
                !should_exclude_path(path) &&
                path.is_file()
            }) {
            total_size += entry.metadata().map_err(|e| format!("Failed to get metadata: {}", e))?.len();
        }

        eprintln!("Total size to archive: {} bytes", total_size);
        if total_size == 0 {
            return Err("No files found to archive".to_string());
        }

        // Add the entire directory to the archive, excluding ignored directories
        let mut current_size = 0;
        for entry in walkdir::WalkDir::new(&from_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let path = e.path();
                // Skip excluded directories and their contents
                !should_exclude_path(path) &&
                path.is_file()
            }) {
            let path = entry.path();
            // Get the relative path from the source directory
            let name = path.strip_prefix(&from_path)
                .map_err(|e| format!("Failed to get relative path: {}", e))?
                .to_str()
                .ok_or_else(|| "Invalid path encoding".to_string())?;

            let file_size = entry.metadata().map_err(|e| format!("Failed to get file metadata: {}", e))?.len();
            eprintln!("Adding file to archive: {} ({} bytes)", name, file_size);

            let mut file = fs::File::open(path).map_err(|e| format!("Failed to open source file: {}", e))?;

            // Create a new header with the correct path
            let mut header = tar::Header::new_gnu();
            header.set_path(name).map_err(|e| format!("Failed to set path in header: {}", e))?;
            header.set_size(file_size);
            header.set_cksum();

            // Write the header and file contents
            builder.append(&header, &mut file).map_err(|e| format!("Failed to append file to archive: {}", e))?;
            sync_to_disk();

            current_size += file_size;
            progress.store((current_size * 100 / total_size) as u16, Ordering::SeqCst);
        }

        eprintln!("Finished creating archive, final size: {} bytes", current_size);
        if current_size == 0 {
            return Err("No files were added to the archive".to_string());
        }

        builder.finish().map_err(|e| format!("Failed to finish archive: {}", e))?;
        sync_to_disk();

        // Verify the archive was created and has content
        let archive_size = fs::metadata(&to_path_tar).map_err(|e| format!("Failed to get archive metadata: {}", e))?.len();
        eprintln!("Archive file size: {} bytes", archive_size);
        if archive_size == 0 {
            return Err("Created archive is empty".to_string());
        }

        Ok(())
    } else if to_drive == "internal" {
        // External to internal: extract tar archive
        eprintln!("Starting external to internal copy for {}", cart_id);
        fs::create_dir_all(&to_path).map_err(|e| format!("Failed to create destination directory: {}", e))?;

        let file = fs::File::open(&from_path_tar).map_err(|e| format!("Failed to open source archive: {}", e))?;
        let file_size = file.metadata().map_err(|e| format!("Failed to get archive metadata: {}", e))?.len();
        eprintln!("Archive size: {} bytes", file_size);

        let mut archive = Archive::new(file);
        let mut current_size = 0;

        for entry in archive.entries().map_err(|e| format!("Failed to read archive entries: {}", e))? {
            let mut entry = entry.map_err(|e| format!("Failed to read archive entry: {}", e))?;
            let path = entry.path().map_err(|e| format!("Failed to get entry path: {}", e))?;
            let entry_size = entry.header().size().unwrap_or(0);
            eprintln!("Extracting: {} ({} bytes)", path.display(), entry_size);

            // Ensure the parent directory exists
            if let Some(parent) = path.parent() {
                fs::create_dir_all(to_path.join(parent))
                    .map_err(|e| format!("Failed to create parent directory: {}", e))?;
            }

            // Extract the file
            entry.unpack_in(&to_path)
                .map_err(|e| format!("Failed to extract file: {}", e))?;

            current_size += entry_size;
            progress.store((current_size * 100 / file_size) as u16, Ordering::SeqCst);
        }

        // Verify extraction
        let mut extracted_size = 0;
        for entry in walkdir::WalkDir::new(&to_path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file()) {
            extracted_size += entry.metadata()
                .map_err(|e| format!("Failed to get extracted file metadata: {}", e))?
                .len();
        }
        eprintln!("Total extracted size: {} bytes", extracted_size);

        if extracted_size == 0 {
            return Err("No files were extracted from the archive".to_string());
        }

        Ok(())
    } else {
        // External to external: direct copy with progress
        let file_size = fs::metadata(&from_path_tar).map_err(|e| e.to_string())?.len();
        let mut source = fs::File::open(&from_path_tar).map_err(|e| e.to_string())?;
        let mut dest = fs::File::create(&to_path_tar).map_err(|e| e.to_string())?;

        let mut buffer = [0; 8192];
        let mut current_size = 0;
        loop {
            let bytes_read = source.read(&mut buffer).map_err(|e| e.to_string())?;
            if bytes_read == 0 {
                break;
            }
            dest.write_all(&buffer[..bytes_read]).map_err(|e| e.to_string())?;
            sync_to_disk();

            current_size += bytes_read as u64;
            progress.store((current_size * 100 / file_size) as u16, Ordering::SeqCst);
        }
        Ok(())
    };

    // If the main copy operation failed, clean up and return error
    if let Err(e) = result {
        // Clean up by removing the top-level directories
        if to_drive == "internal" {
            fs::remove_dir_all(&to_path).ok();
        } else {
            fs::remove_file(&to_path_tar).ok();
        }
        fs::remove_dir_all(Path::new(&to_cache).join(cart_id)).ok();
        return Err(e);
    }

    // Copy cache files
    let to_cache_path = Path::new(&to_cache).join(cart_id);
    fs::remove_dir_all(&to_cache_path).ok(); // Ignore errors if directory doesn't exist
    fs::create_dir_all(&to_cache_path).map_err(|e| e.to_string())?;

    // Copy metadata.kzi if it exists
    let from_metadata = Path::new(&from_cache).join(cart_id).join("metadata.kzi");
    let to_metadata = to_cache_path.join("metadata.kzi");
    if from_metadata.exists() {
        fs::copy(&from_metadata, &to_metadata).map_err(|e| e.to_string())?;
    }

    // Copy icon.png if it exists
    let from_icon = Path::new(&from_cache).join(cart_id).join("icon.png");
    let to_icon = to_cache_path.join("icon.png");
    if from_icon.exists() {
        fs::copy(&from_icon, &to_icon).map_err(|e| e.to_string())?;
    }

    sync_to_disk();
    Ok(())
}

/// Calculate total playtime for a game from its .kazeta/var/playtime.log file
/// Returns playtime in hours with one decimal place
pub fn calculate_playtime(cart_id: &str, drive_name: &str) -> f32 {
    println!("Calculating playtime for {} on {}", cart_id, drive_name);
    let save_dir = get_save_dir_from_drive_name(drive_name);

    // Check if this is a tar file (external drive) or directory (internal drive)
    let tar_path = Path::new(&save_dir).join(format!("{}.tar", cart_id));
    let dir_path = Path::new(&save_dir).join(cart_id);

    if tar_path.exists() {
        // External drive: read from tar archive
        calculate_playtime_from_tar(&tar_path, cart_id)
    } else if dir_path.exists() {
        // Internal drive: read from directory
        calculate_playtime_from_dir(&dir_path, cart_id)
    } else {
        // Neither exists
        0.0
    }
}

/// Calculate playtime from a tar archive (external drives)
fn calculate_playtime_from_tar(tar_path: &Path, _cart_id: &str) -> f32 {
    let file = match fs::File::open(tar_path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("Failed to open tar file {}: {}", tar_path.display(), e);
            return 0.0;
        }
    };

    let mut archive = tar::Archive::new(file);
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Failed to read archive entries: {}", e);
            return 0.0;
        }
    };

    let mut content = String::new();
    let mut start_content = String::new();
    let mut end_content = String::new();

    for entry_result in entries {
        let mut entry = match entry_result {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("Failed to read tar entry: {}", e);
                continue;
            }
        };

        let path = match entry.path() {
            Ok(path) => path,
            Err(e) => {
                eprintln!("Failed to get tar entry path: {}", e);
                continue;
            }
        };

        if path.display().to_string() == ".kazeta/var/playtime.log" {
            let _ = entry.read_to_string(&mut content);
        } else if path.display().to_string() == ".kazeta/var/playtime_start" {
            let _ = entry.read_to_string(&mut start_content);
        } else if path.display().to_string() == ".kazeta/var/playtime_end" {
            let _ = entry.read_to_string(&mut end_content);
        }
    }

    parse_playtime_content(&format!("{}\n{} {}", content.trim(), start_content.trim(), end_content.trim()))
}

/// Calculate playtime from a directory (internal drives)
fn calculate_playtime_from_dir(dir_path: &Path, _cart_id: &str) -> f32 {
    let playtime_log_path = dir_path.join(".kazeta/var/playtime.log");
    let playtime_start_path = dir_path.join(".kazeta/var/playtime_start");
    let playtime_end_path = dir_path.join(".kazeta/var/playtime_end");

    let content = match fs::read_to_string(&playtime_log_path) {
        Ok(content) => content.trim().to_string(),
        Err(_) => "".to_string(),
    };

    let start_content = match fs::read_to_string(&playtime_start_path) {
        Ok(content) => content.trim().to_string(),
        Err(_) => "".to_string(),
    };

    let end_content = match fs::read_to_string(&playtime_end_path) {
        Ok(content) => content.trim().to_string(),
        Err(_) => "".to_string(),
    };

    return parse_playtime_content(&format!("{}\n{} {}", content.trim(), start_content.trim(), end_content.trim()));
}

/// Parse playtime content from a string (common logic for both tar and directory)
fn parse_playtime_content(content: &str) -> f32 {
    let mut total_seconds: i64 = 0;

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }

        let start_time = match DateTime::parse_from_rfc3339(parts[0]) {
            Ok(dt) => dt,
            Err(e) => {
                eprintln!("Failed to parse start time '{}': {}", parts[0], e);
                continue;
            }
        };

        let end_time = match DateTime::parse_from_rfc3339(parts[1]) {
            Ok(dt) => dt,
            Err(e) => {
                eprintln!("Failed to parse end time '{}': {}", parts[1], e);
                continue;
            }
        };

        let duration = end_time.signed_duration_since(start_time);
        total_seconds += duration.num_seconds();
    }

    // Convert to hours rounded to one decimal place
    ((total_seconds as f64 / 360.0).round() / 10.0) as f32
}

/// Calculate save data size for a game (lazy calculation)
/// Returns size in MB with one decimal place
pub fn calculate_save_size(cart_id: &str, drive_name: &str) -> f32 {
    println!("Calculating save size for {} on {}", cart_id, drive_name);
    let save_dir = get_save_dir_from_drive_name(drive_name);

    // Check if this is a tar file (external drive) or directory (internal drive)
    let tar_path = Path::new(&save_dir).join(format!("{}.tar", cart_id));
    let dir_path = Path::new(&save_dir).join(cart_id);

    let size_bytes = if tar_path.exists() {
        // External drive: get tar file size
        calculate_size_from_tar(&tar_path)
    } else if dir_path.exists() {
        // Internal drive: calculate directory size
        calculate_size_from_dir(&dir_path)
    } else {
        // Neither exists
        return 0.0;
    };

    // Convert to MB with one decimal place, rounding up to nearest 0.1 MB if non-zero
    let size_mb = size_bytes as f64 / 1024.0 / 1024.0;
    if size_mb > 0.0 {
        ((size_mb * 10.0).ceil() / 10.0) as f32
    } else {
        0.0
    }
}

/// Calculate size from a tar archive (external drives)
fn calculate_size_from_tar(tar_path: &Path) -> u64 {
    let metadata = match fs::metadata(tar_path) {
        Ok(metadata) => metadata,
        Err(e) => {
            eprintln!("Failed to get tar file metadata: {}", e);
            return 0;
        }
    };
    metadata.len()
}

/// Calculate size from a directory (internal drives)
fn calculate_size_from_dir(dir_path: &Path) -> u64 {
    let mut total_size = 0u64;

    for entry in walkdir::WalkDir::new(dir_path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            // Skip excluded directories and their contents
            !should_exclude_path(path) &&
            path.is_file()
        }) {
        if let Ok(metadata) = entry.metadata() {
            total_size += metadata.len();
        }
    }
    total_size
}

fn sync_to_disk() {
    if let Ok(output) = Command::new("sync")
        .output()
        .map_err(|e| format!("Failed to execute sync command: {}", e)) {

        if !output.status.success() {
            println!("Sync command failed with status: {}", output.status);
        }
    }
}