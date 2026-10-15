// Standard volume key handling for the Kazeta OS session.
// Reads volume keys straight from the input devices so they work while a game has focus,
// and adjusts the default audio sink through wpctl. There is no UI beyond a sound effect played with pw-play.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

use evdev::{Device, InputEventKind, Key};

/// Set by kazeta-session to enable volume key handling; desktop sessions leave volume keys to the desktop environment
const ENABLE_ENV: &str = "KAZETA_HANDLE_VOLUME_KEYS";

const SINK: &str = "@DEFAULT_AUDIO_SINK@";
const STEP: &str = "5%";
const MAX_VOLUME: &str = "1.0";
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
const SOUND: &[u8] = include_bytes!("../move.wav");

/// pw-play needs a file, so the embedded sound is written out to the runtime directory
fn sound_file() -> PathBuf {
    crate::launcher::runtime_dir().join("kazeta-volume.wav")
}

/// Start handling volume keys if enabled for this session
pub fn watch() {
    if !std::env::var_os(ENABLE_ENV).is_some_and(|v| v == "1") {
        return;
    }
    if let Err(e) = fs::write(sound_file(), SOUND) {
        eprintln!("Failed to write volume sound to {}: {}", sound_file().display(), e);
    }
    thread::spawn(|| {
        let mut watched: Vec<String> = Vec::new();
        // keyboards can be hot-plugged, so keep looking for new devices
        loop {
            for (path, device) in evdev::enumerate() {
                let path = path.to_string_lossy().into_owned();
                if watched.contains(&path) || !has_volume_keys(&device) {
                    continue;
                }
                watched.push(path.clone());
                eprintln!("Watching volume keys on {} ({})", path, device.name().unwrap_or("unknown"));
                thread::spawn(move || {
                    if let Err(e) = handle_device(device) {
                        eprintln!("Stopped watching volume keys on {}: {}", path, e);
                    }
                });
            }
            thread::sleep(RESCAN_INTERVAL);
        }
    });
}

fn has_volume_keys(device: &Device) -> bool {
    device.supported_keys().is_some_and(|keys| {
        keys.contains(Key::KEY_VOLUMEUP) || keys.contains(Key::KEY_VOLUMEDOWN) || keys.contains(Key::KEY_MUTE)
    })
}

fn handle_device(mut device: Device) -> std::io::Result<()> {
    loop {
        for event in device.fetch_events()? {
            // value 1 is a press and 2 is a key repeat, which lets holding a key keep adjusting
            if event.value() == 0 {
                continue;
            }
            let InputEventKind::Key(key) = event.kind() else { continue };
            match key {
                Key::KEY_VOLUMEUP => wpctl(&["set-volume", "-l", MAX_VOLUME, SINK, &format!("{}+", STEP)]),
                Key::KEY_VOLUMEDOWN => wpctl(&["set-volume", SINK, &format!("{}-", STEP)]),
                Key::KEY_MUTE if event.value() == 1 => wpctl(&["set-mute", SINK, "toggle"]),
                _ => continue,
            }
            // played after the change so it is heard at the new volume
            play_sound();
        }
    }
}

fn wpctl(args: &[&str]) {
    if let Err(e) = Command::new("wpctl").args(args).status() {
        eprintln!("Failed to run wpctl: {}", e);
    }
}

/// Play the volume sound without waiting for it to finish, so holding a key keeps adjusting
fn play_sound() {
    thread::spawn(|| {
        if let Err(e) = Command::new("pw-play").arg(sound_file()).status() {
            eprintln!("Failed to run pw-play: {}", e);
        }
    });
}
