use macroquad::{audio, prelude::*};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time;
use std::collections::HashMap;
use gilrs::{Gilrs, Button, Axis};
use std::panic;
use futures;
use std::sync::atomic::{AtomicU16, Ordering, AtomicBool};
use std::fs;
use std::path::Path;
use std::process;

mod save;

const SCREEN_WIDTH: i32 = 640;
const SCREEN_HEIGHT: i32 = 360;
const TILE_SIZE: f32 = 32.0;
const PADDING: f32 = 16.0;
const FONT_SIZE: u16 = 16;
const GRID_OFFSET: f32 = 52.0;
const GRID_WIDTH: usize = 13;
const GRID_HEIGHT: usize = 5;
const UI_BG_COLOR: Color = Color {r: 0.0, g: 0.0, b: 0.0, a: 0.5 };
const UI_BG_COLOR_DARK: Color = Color {r: 0.0, g: 0.0, b: 0.0, a: 0.3 };
const UI_BG_COLOR_DIALOG: Color = Color {r: 0.0, g: 0.0, b: 0.0, a: 0.8 };
const SELECTED_OFFSET: f32 = 5.0;

struct SoundEffects {
    cursor_move: audio::Sound,
    select: audio::Sound,
    reject: audio::Sound,
    back: audio::Sound,
}

impl SoundEffects {
    async fn new() -> Self {
        SoundEffects {
            cursor_move: audio::load_sound_from_bytes(include_bytes!("../move.wav")).await.unwrap(),
            select: audio::load_sound_from_bytes(include_bytes!("../select.wav")).await.unwrap(),
            reject: audio::load_sound_from_bytes(include_bytes!("../reject.wav")).await.unwrap(),
            back: audio::load_sound_from_bytes(include_bytes!("../back.wav")).await.unwrap(),
        }
    }

    fn play_cursor_move(&self) {
        audio::play_sound(&self.cursor_move, audio::PlaySoundParams {
            looped: false,
            volume: 0.25,
        });
    }

    fn play_select(&self) {
        audio::play_sound(&self.select, audio::PlaySoundParams {
            looped: false,
            volume: 0.75,
        });
    }

    fn play_reject(&self) {
        audio::play_sound(&self.reject, audio::PlaySoundParams {
            looped: false,
            volume: 0.75,
        });
    }

    fn play_back(&self) {
        audio::play_sound(&self.back, audio::PlaySoundParams {
            looped: false,
            volume: 0.5,
        });
    }
}

fn window_conf() -> Conf {
    Conf {
        window_title: "kazeta-bios".to_owned(),
        window_resizable: false,
        window_width: SCREEN_WIDTH,
        window_height: SCREEN_HEIGHT,
        high_dpi: false,
        fullscreen: false,

        ..Default::default()
    }
}

#[derive(Clone, Debug)]
struct Memory {
    id: String,
    name: Option<String>,
    drive_name: String, // Store which drive this save is on
}

#[derive(Clone, Debug)]
struct StorageMedia {
    id: String,
    free: u32,
}

struct DialogOption {
    text: String,
    value: String,
    disabled: bool,
}

struct Dialog {
    id: String,
    desc: Option<String>,
    options: Vec<DialogOption>,
    selection: usize,
}

struct CopyOperationState {
    progress: u16,
    running: bool,
    should_clear_dialogs: bool,
    error_message: Option<String>,
}

struct DrawContext {
    font: Font,
}

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    MainMenu,
    SaveData,
    FadingOut,
}

// UI Focus for Save Data Screen
#[derive(Clone, Debug, PartialEq)]
enum UIFocus {
    Grid,
    StorageLeft,
    StorageRight,
}

#[derive(Clone, Debug, PartialEq)]
enum ShakeTarget {
    None,
    LeftArrow,
    RightArrow,
    Dialog,
    PlayOption,
}

struct AnimationState {
    shake_time: f32,  // Current shake animation time
    shake_target: ShakeTarget, // Which element is currently shaking
    cursor_animation_time: f32, // Time counter for cursor animations
    cursor_transition_time: f32, // Time counter for cursor transition animation
    dialog_transition_time: f32, // Time counter for dialog transition animation
    dialog_transition_progress: f32, // Progress of dialog transition (0.0 to 1.0)
    dialog_transition_start_pos: Vec2, // Starting position for icon transition
    dialog_transition_end_pos: Vec2, // Ending position for icon transition
}

impl AnimationState {
    const SHAKE_DURATION: f32 = 0.2;    // Duration of shake animation in seconds
    const SHAKE_INTENSITY: f32 = 3.0;   // How far the arrow shakes
    const CURSOR_ANIMATION_SPEED: f32 = 10.0; // Speed of cursor color animation
    const CURSOR_TRANSITION_DURATION: f32 = 0.15; // Duration of cursor transition animation
    const DIALOG_TRANSITION_DURATION: f32 = 0.4; // Duration of dialog transition animation

    fn new() -> Self {
        AnimationState {
            shake_time: 0.0,
            shake_target: ShakeTarget::None,
            cursor_animation_time: 0.0,
            cursor_transition_time: 0.0,
            dialog_transition_time: 0.0,
            dialog_transition_progress: 0.0,
            dialog_transition_start_pos: Vec2::ZERO,
            dialog_transition_end_pos: Vec2::ZERO,
        }
    }

    fn calculate_shake_offset(&self, target: ShakeTarget) -> f32 {
        if self.shake_target == target && self.shake_time > 0.0 {
            (self.shake_time / Self::SHAKE_DURATION * std::f32::consts::PI * 8.0).sin() * Self::SHAKE_INTENSITY
        } else {
            0.0
        }
    }

    fn update_shake(&mut self, delta_time: f32) {
        // Update shake animation
        if self.shake_time > 0.0 {
            self.shake_time = (self.shake_time - delta_time).max(0.0);
            if self.shake_time <= 0.0 {
                self.shake_target = ShakeTarget::None;
            }
        }
    }

    fn update_cursor_animation(&mut self, delta_time: f32) {
        // Update cursor animation
        self.cursor_animation_time = (self.cursor_animation_time + delta_time * Self::CURSOR_ANIMATION_SPEED) % (2.0 * std::f32::consts::PI);
        // Update cursor transition
        if self.cursor_transition_time > 0.0 {
            self.cursor_transition_time = (self.cursor_transition_time - delta_time).max(0.0);
        }
    }

    fn trigger_shake(&mut self, is_left: bool) {
        if is_left {
            self.shake_target = ShakeTarget::LeftArrow;
            self.shake_time = Self::SHAKE_DURATION;
        } else {
            self.shake_target = ShakeTarget::RightArrow;
            self.shake_time = Self::SHAKE_DURATION;
        }
    }

    fn trigger_dialog_shake(&mut self) {
        self.shake_target = ShakeTarget::Dialog;
        self.shake_time = Self::SHAKE_DURATION;
    }

    fn trigger_play_option_shake(&mut self) {
        self.shake_target = ShakeTarget::PlayOption;
        self.shake_time = Self::SHAKE_DURATION;
    }

    fn trigger_transition(&mut self) {
        self.cursor_transition_time = Self::CURSOR_TRANSITION_DURATION;
    }

    fn get_cursor_color(&self) -> Color {
        let c = (self.cursor_animation_time.sin() * 0.5 + 0.5).max(0.3);
        Color { r: c, g: c, b: c, a: c }
    }

    fn get_cursor_scale(&self) -> f32 {
        if self.cursor_transition_time > 0.0 {
            let t = self.cursor_transition_time / Self::CURSOR_TRANSITION_DURATION;
            // Start at 1.5x size and smoothly transition to 1.0x
            1.0 + 0.5 * t
        } else {
            1.0
        }
    }

    fn update_dialog_transition(&mut self, delta_time: f32) {
        if self.dialog_transition_time > 0.0 {
            self.dialog_transition_time = (self.dialog_transition_time - delta_time).max(0.0);
            self.dialog_transition_progress = 1.0 - (self.dialog_transition_time / Self::DIALOG_TRANSITION_DURATION);
        }
    }

    fn trigger_dialog_transition(&mut self, start_pos: Vec2, end_pos: Vec2) {
        self.dialog_transition_time = Self::DIALOG_TRANSITION_DURATION;
        self.dialog_transition_progress = 0.0;
        self.dialog_transition_start_pos = start_pos;
        self.dialog_transition_end_pos = end_pos;
    }

    fn get_dialog_transition_pos(&self) -> Vec2 {
        let t = self.dialog_transition_progress;
        // Use smooth easing function
        let t = t * t * (3.0 - 2.0 * t);
        self.dialog_transition_start_pos.lerp(self.dialog_transition_end_pos, t)
    }
}

struct InputState {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    select: bool,
    next: bool,
    prev: bool,
    cycle: bool,
    back: bool,
    analog_was_neutral: bool,
    ui_focus: UIFocus,
}

impl InputState {
    const ANALOG_DEADZONE: f32 = 0.5;  // Increased deadzone for less sensitivity

    fn new() -> Self {
        InputState {
            up: false,
            down: false,
            left: false,
            right: false,
            select: false,
            next: false,
            prev: false,
            cycle: false,
            back: false,
            analog_was_neutral: true,
            ui_focus: UIFocus::Grid,
        }
    }

    fn update_keyboard(&mut self) {
        self.up = is_key_pressed(KeyCode::Up);
        self.down = is_key_pressed(KeyCode::Down);
        self.left = is_key_pressed(KeyCode::Left);
        self.right = is_key_pressed(KeyCode::Right);
        self.select = is_key_pressed(KeyCode::Enter);
        self.next = is_key_pressed(KeyCode::RightBracket);
        self.prev = is_key_pressed(KeyCode::LeftBracket);
        self.back = is_key_pressed(KeyCode::Backspace);
        self.cycle = is_key_pressed(KeyCode::Tab);
    }

    fn update_controller(&mut self, gilrs: &mut Gilrs) {
        // Handle button events
        while let Some(ev) = gilrs.next_event() {
            match ev.event {
                gilrs::EventType::ButtonPressed(Button::DPadUp, _) => self.up = true,
                gilrs::EventType::ButtonReleased(Button::DPadUp, _) => self.up = false,
                gilrs::EventType::ButtonPressed(Button::DPadDown, _) => self.down = true,
                gilrs::EventType::ButtonReleased(Button::DPadDown, _) => self.down = false,
                gilrs::EventType::ButtonPressed(Button::DPadLeft, _) => self.left = true,
                gilrs::EventType::ButtonReleased(Button::DPadLeft, _) => self.left = false,
                gilrs::EventType::ButtonPressed(Button::DPadRight, _) => self.right = true,
                gilrs::EventType::ButtonReleased(Button::DPadRight, _) => self.right = false,
                gilrs::EventType::ButtonPressed(Button::South, _) => self.select = true,
                gilrs::EventType::ButtonReleased(Button::South, _) => self.select = false,
                gilrs::EventType::ButtonPressed(Button::RightTrigger, _) => self.next = true,
                gilrs::EventType::ButtonReleased(Button::RightTrigger, _) => self.next = false,
                gilrs::EventType::ButtonPressed(Button::LeftTrigger, _) => self.prev = true,
                gilrs::EventType::ButtonReleased(Button::LeftTrigger, _) => self.prev = false,
                gilrs::EventType::ButtonPressed(Button::East, _) => self.back = true,
                gilrs::EventType::ButtonReleased(Button::East, _) => self.back = false,
                _ => {}
            }
        }

        // Handle analog stick input
        for (_, gamepad) in gilrs.gamepads() {
            let x = gamepad.value(Axis::LeftStickX);
            let y = gamepad.value(Axis::LeftStickY);

            // Apply deadzone to analog values
            let apply_deadzone = |value: f32| {
                if value.abs() < Self::ANALOG_DEADZONE {
                    0.0
                } else {
                    value
                }
            };

            let x = apply_deadzone(x);
            let y = apply_deadzone(y);

            // Check if stick is in neutral position
            let is_neutral = x.abs() < Self::ANALOG_DEADZONE && y.abs() < Self::ANALOG_DEADZONE;

            // Only trigger movement if stick was in neutral position last frame
            if self.analog_was_neutral {
                self.up = self.up || y > Self::ANALOG_DEADZONE;
                self.down = self.down || y < -Self::ANALOG_DEADZONE;
                self.left = self.left || x < -Self::ANALOG_DEADZONE;
                self.right = self.right || x > Self::ANALOG_DEADZONE;
            }

            // Update neutral state for next frame
            self.analog_was_neutral = is_neutral;
        }
    }
}

fn pixel_pos(v: f32) -> f32 {
    PADDING + v*TILE_SIZE + v*PADDING
}

fn copy_memory(memory: &Memory, from_media: &StorageMedia, to_media: &StorageMedia, state: Arc<Mutex<CopyOperationState>>) {
    // Initialize the copy operation state
    if let Ok(mut copy_state) = state.lock() {
        copy_state.progress = 0;
        copy_state.running = true;
        copy_state.error_message = None;
    }

    // Small delay to show the operation has started
    thread::sleep(time::Duration::from_millis(500));

    // Create progress tracking
    let progress = Arc::new(AtomicU16::new(0));
    let progress_clone = progress.clone();
    let state_clone = state.clone();

    // Spawn a thread to monitor progress from the copy operation
    let monitor_handle = thread::spawn(move || {
        loop {
            let current_progress = progress_clone.load(Ordering::SeqCst);

            // Update the UI state with the current progress
            if let Ok(mut copy_state) = state_clone.lock() {
                // Only update if the operation is still running
                if copy_state.running {
                    copy_state.progress = current_progress;
                } else {
                    // Operation completed, exit the monitoring loop
                    break;
                }
            }

            // If we've reached 100%, the copy operation should be finishing soon
            if current_progress >= 100 {
                break;
            }

            thread::sleep(time::Duration::from_millis(50));
        }
    });

    // Perform the actual copy operation
    let copy_result = save::copy_save(&memory.id, &from_media.id, &to_media.id, progress);

    // Handle the result
    match copy_result {
        Ok(_) => {
            // Ensure progress shows 100% on success
            if let Ok(mut copy_state) = state.lock() {
                copy_state.progress = 100;
            }

            // Pause for 1.5 seconds to show completion clearly while keeping the operation running
            thread::sleep(time::Duration::from_millis(1500));

            // Mark operation as complete (this will allow the monitoring thread to exit)
            if let Ok(mut copy_state) = state.lock() {
                copy_state.running = false;
                copy_state.should_clear_dialogs = true;
            }

            // Wait for the monitoring thread to finish
            monitor_handle.join().ok();
        },
        Err(e) => {
            // Handle error case (this will also stop the monitoring thread)
            if let Ok(mut copy_state) = state.lock() {
                copy_state.running = false;
                copy_state.should_clear_dialogs = true;
                copy_state.error_message = Some(format!("Failed to copy save: {}", e));
            }

            // Wait for the monitoring thread to finish
            monitor_handle.join().ok();
        }
    }
}

async fn load_memories(media: &StorageMedia, cache: &mut HashMap<String, Texture2D>, queue: &mut Vec<(String, String)>) -> Vec<Memory> {
    let mut memories = Vec::new();

    if let Ok(details) = save::get_save_details(&media.id) {
        for (cart_id, name, icon_path) in details {
            if !cache.contains_key(&cart_id) {
                queue.push((cart_id.clone(), icon_path.clone()));
            }

            let m = Memory {
                id: cart_id,
                name: Some(name),
                drive_name: media.id.clone(),
            };
            memories.push(m);
        }
    }

    memories
}

fn text(ctx : &DrawContext, text : &str, x : f32, y: f32) {
    draw_text_ex(&text.to_uppercase(), x+1.0, y+1.0, TextParams {
        font: Some(&ctx.font),
        font_size: FONT_SIZE,
        color: Color {r:0.0, g:0.0, b:0.0, a:0.9},
        ..Default::default()
    });
    draw_text_ex(&text.to_uppercase(), x, y, TextParams {
        font: Some(&ctx.font),
        font_size: FONT_SIZE,
        ..Default::default()
    });
}

fn text_disabled(ctx : &DrawContext, text : &str, x : f32, y: f32) {
    draw_text_ex(&text.to_uppercase(), x+1.0, y+1.0, TextParams {
        font: Some(&ctx.font),
        font_size: FONT_SIZE,
        color: Color {r:0.0, g:0.0, b:0.0, a:1.0},
        ..Default::default()
    });
    draw_text_ex(&text.to_uppercase(), x, y, TextParams {
        font: Some(&ctx.font),
        font_size: FONT_SIZE,
        color: Color {r:0.4, g:0.4, b:0.4, a:1.0},
        ..Default::default()
    });
}

#[derive(Clone, Debug)]
struct StorageMediaState {

    // all storage media, including disabled media
    all_media: Vec<StorageMedia>,

    // media that can actually be used
    media: Vec<StorageMedia>,

    // the index of selection in 'media'
    selected: usize,

    needs_memory_refresh: bool,
}

impl StorageMediaState {
    fn new() -> Self {
        StorageMediaState {
            all_media: Vec::new(),
            media: Vec::new(),
            selected: 0,
            needs_memory_refresh: false,
        }
    }

    fn update_media(&mut self) {
        let mut all_new_media = Vec::new();

        if let Ok(devices) = save::list_devices() {
            for (id, free) in devices {
                all_new_media.push(StorageMedia {
                    id,
                    free,
                });
            }
        }

        // Done if media list has not changed
        if self.all_media.len() == all_new_media.len() &&
           !self.all_media.iter().zip(all_new_media.iter()).any(|(a, b)| a.id != b.id) {

            //  update free space
            self.all_media = all_new_media;
            for media in &mut self.media {
                if let Some(pos) = self.all_media.iter().position(|m| m.id == media.id) {
                    media.free = self.all_media.get(pos).unwrap().free
                }
            }

            return;
        }

        let new_media: Vec<StorageMedia> = all_new_media
            .clone()
            .into_iter()
            .filter(|m| save::has_save_dir(&m.id) && !save::is_cart(&m.id))
            .collect();

        // Try to keep the same device selected if it still exists
        let mut new_pos = 0;
        if let Some(old_selected_media) = self.media.get(self.selected) {
            if let Some(pos) = new_media.iter().position(|m| m.id == old_selected_media.id) {
                new_pos = pos;
            }
        }

        self.all_media = all_new_media;
        self.media = new_media;
        self.selected = new_pos;
        self.needs_memory_refresh = true;
    }
}

/// Get playtime for a specific game, using cache when available
fn get_game_playtime(memory: &Memory, playtime_cache: &mut PlaytimeCache) -> f32 {
    let cache_key = (memory.id.clone(), memory.drive_name.clone());

    if let Some(&cached_playtime) = playtime_cache.get(&cache_key) {
        cached_playtime
    } else {
        let calculated_playtime = save::calculate_playtime(&memory.id, &memory.drive_name);
        playtime_cache.insert(cache_key, calculated_playtime);
        calculated_playtime
    }
}

/// Get size for a specific game, using cache when available
fn get_game_size(memory: &Memory, size_cache: &mut SizeCache) -> f32 {
    let cache_key = (memory.id.clone(), memory.drive_name.clone());

    if let Some(&cached_size) = size_cache.get(&cache_key) {
        cached_size
    } else {
        let calculated_size = save::calculate_save_size(&memory.id, &memory.drive_name);
        size_cache.insert(cache_key, calculated_size);
        calculated_size
    }
}

// Playtime cache to avoid recalculating playtime for the same game on the same drive
type PlaytimeCacheKey = (String, String); // (cart_id, drive_name)
type PlaytimeCache = HashMap<PlaytimeCacheKey, f32>;

// Size cache to avoid recalculating size for the same game on the same drive
type SizeCacheKey = (String, String); // (cart_id, drive_name)
type SizeCache = HashMap<SizeCacheKey, f32>;

fn get_memory_index(selected_memory: usize, scroll_offset: usize) -> usize {
    selected_memory + GRID_WIDTH * scroll_offset
}

fn calculate_icon_transition_positions(selected_memory: usize) -> (Vec2, Vec2) {
    let xp = (selected_memory % GRID_WIDTH) as f32;
    let yp = (selected_memory / GRID_WIDTH) as f32;
    let grid_pos = Vec2::new(
        pixel_pos(xp),
        pixel_pos(yp) + GRID_OFFSET
    );
    let dialog_pos = Vec2::new(PADDING, PADDING);
    (grid_pos, dialog_pos)
}

fn render_data_view(
    ctx: &DrawContext,
    selected_memory: usize,
    memories: &Vec<Memory>,
    icon_cache: &HashMap<String, Texture2D>,
    storage_state: &Arc<Mutex<StorageMediaState>>,
    placeholder: &Texture2D,
    scroll_offset: usize,
    input_state: &mut InputState,
    animation_state: &mut AnimationState,
    playtime_cache: &mut PlaytimeCache,
    size_cache: &mut SizeCache,
) {
    let xp = (selected_memory % GRID_WIDTH) as f32;
    let yp = (selected_memory / GRID_WIDTH) as f32;

    // Draw grid selection highlight when focused on grid
    if let UIFocus::Grid = input_state.ui_focus {
        let cursor_color = animation_state.get_cursor_color();
        let cursor_thickness = 6.0;
        let cursor_scale = animation_state.get_cursor_scale();

        let base_size = TILE_SIZE + 6.0;
        let scaled_size = base_size * cursor_scale;
        let offset = (scaled_size - base_size) / 2.0;

        draw_rectangle_lines(
            pixel_pos(xp)-3.0-SELECTED_OFFSET - offset,
            pixel_pos(yp)-3.0-SELECTED_OFFSET+GRID_OFFSET - offset,
            scaled_size,
            scaled_size,
            cursor_thickness,
            cursor_color
        );
    }

    for x in 0..GRID_WIDTH {
        for y in 0..GRID_HEIGHT {
            let memory_index = get_memory_index(x + GRID_WIDTH * y, scroll_offset);

            if xp as usize == x && yp as usize == y {
                if let UIFocus::Grid = input_state.ui_focus {
                    draw_rectangle(pixel_pos(x as f32)-SELECTED_OFFSET, pixel_pos(y as f32)-SELECTED_OFFSET+GRID_OFFSET, TILE_SIZE, TILE_SIZE, UI_BG_COLOR);
                } else {
                    draw_rectangle(pixel_pos(x as f32)-2.0, pixel_pos(y as f32)+GRID_OFFSET-2.0, TILE_SIZE+4.0, TILE_SIZE+4.0, UI_BG_COLOR);
                }
            } else {
                draw_rectangle(pixel_pos(x as f32)-2.0, pixel_pos(y as f32)+GRID_OFFSET-2.0, TILE_SIZE+4.0, TILE_SIZE+4.0, UI_BG_COLOR);
            }

            let Some(mem) = memories.get(memory_index) else {
                continue;
            };

            // Skip rendering the icon at its grid position during transitions
            if xp as usize == x && yp as usize == y && animation_state.dialog_transition_time > 0.0 {
                continue;
            }

            let icon = match icon_cache.get(&mem.id) {
                Some(icon) => icon,
                None => placeholder,
            };

            let params = DrawTextureParams {
                dest_size: Some(Vec2 {x: TILE_SIZE, y: TILE_SIZE }),
                source: Some(Rect { x: 0.0, y: 0.0, h: icon.height(), w: icon.width() }),
                rotation: 0.0,
                flip_x: false,
                flip_y: false,
                pivot: None
            };
            if xp as usize == x && yp as usize == y {
                if let UIFocus::Grid = input_state.ui_focus {
                    draw_texture_ex(&icon, pixel_pos(x as f32)-SELECTED_OFFSET, pixel_pos(y as f32)-SELECTED_OFFSET+GRID_OFFSET, WHITE, params);
                } else {
                    draw_texture_ex(&icon, pixel_pos(x as f32), pixel_pos(y as f32)+GRID_OFFSET, WHITE, params);
                }
            } else {
                draw_texture_ex(&icon, pixel_pos(x as f32), pixel_pos(y as f32)+GRID_OFFSET, WHITE, params);
            }
        }
    }

    // Storage media info area with navigation
    const STORAGE_INFO_WIDTH: f32 = 512.0;
    const STORAGE_INFO_X: f32 = TILE_SIZE*2.0;
    const STORAGE_INFO_Y: f32 = 16.0;
    const STORAGE_INFO_HEIGHT: f32 = 36.0;
    const NAV_ARROW_SIZE: f32 = 10.0;
    const NAV_ARROW_OUTLINE: f32 = 1.0;

    // Draw storage info background
    draw_rectangle(STORAGE_INFO_X, STORAGE_INFO_Y, STORAGE_INFO_WIDTH, STORAGE_INFO_HEIGHT, UI_BG_COLOR);
    draw_rectangle_lines(STORAGE_INFO_X-4.0, STORAGE_INFO_Y-4.0, STORAGE_INFO_WIDTH+8.0, STORAGE_INFO_HEIGHT+8.0, 4.0, UI_BG_COLOR_DARK);

    if let Ok(state) = storage_state.lock() {
        if !state.media.is_empty() {
            // Draw left arrow background
            let left_box_x = PADDING;  // Align with leftmost grid column
            let left_box_y = STORAGE_INFO_Y + STORAGE_INFO_HEIGHT/2.0 - TILE_SIZE/2.0;
            let left_shake = animation_state.calculate_shake_offset(ShakeTarget::LeftArrow);

            if let UIFocus::StorageLeft = input_state.ui_focus {
                let cursor_color = animation_state.get_cursor_color();
                let cursor_thickness = 6.0;
                let cursor_scale = animation_state.get_cursor_scale();

                let base_size = TILE_SIZE + 6.0;
                let scaled_size = base_size * cursor_scale;
                let offset = (scaled_size - base_size) / 2.0;

                draw_rectangle(left_box_x-SELECTED_OFFSET + left_shake, left_box_y-SELECTED_OFFSET, TILE_SIZE, TILE_SIZE, UI_BG_COLOR);
                draw_rectangle_lines(
                    left_box_x-3.0-SELECTED_OFFSET + left_shake - offset,
                    left_box_y-3.0-SELECTED_OFFSET - offset,
                    scaled_size,
                    scaled_size,
                    cursor_thickness,
                    cursor_color
                );
            } else {
                draw_rectangle(left_box_x-2.0 + left_shake, left_box_y-2.0, TILE_SIZE+4.0, TILE_SIZE+4.0, UI_BG_COLOR);
            }

            let left_offset = if let UIFocus::StorageLeft = input_state.ui_focus {
                SELECTED_OFFSET
            } else {
                0.0
            };

            let left_points = [
                Vec2::new(4.0 + left_box_x + TILE_SIZE/2.0 - NAV_ARROW_SIZE - left_offset + left_shake, left_box_y + TILE_SIZE/2.0 - left_offset),
                Vec2::new(4.0 + left_box_x + TILE_SIZE/2.0 - left_offset + left_shake, left_box_y + TILE_SIZE/2.0 - NAV_ARROW_SIZE - left_offset),
                Vec2::new(4.0 + left_box_x + TILE_SIZE/2.0 - left_offset + left_shake, left_box_y + TILE_SIZE/2.0 + NAV_ARROW_SIZE - left_offset),
            ];
            let left_color = if state.selected > 0 {
                WHITE
            } else {
                Color { r: 0.3, g: 0.3, b: 0.3, a: 1.0 } // Dark gray when disabled
            };
            draw_triangle(left_points[0], left_points[1], left_points[2], left_color);
            draw_triangle_lines(left_points[0], left_points[1], left_points[2], NAV_ARROW_OUTLINE, BLACK);

            // Draw right arrow background
            let right_box_x = PADDING + (GRID_WIDTH as f32 - 1.0) * (TILE_SIZE + PADDING);  // Align with rightmost grid column
            let right_box_y = STORAGE_INFO_Y + STORAGE_INFO_HEIGHT/2.0 - TILE_SIZE/2.0;
            let right_shake = animation_state.calculate_shake_offset(ShakeTarget::RightArrow);

            if let UIFocus::StorageRight = input_state.ui_focus {
                let cursor_color = animation_state.get_cursor_color();
                let cursor_thickness = 6.0;
                let cursor_scale = animation_state.get_cursor_scale();

                let base_size = TILE_SIZE + 6.0;
                let scaled_size = base_size * cursor_scale;
                let offset = (scaled_size - base_size) / 2.0;

                draw_rectangle(right_box_x-SELECTED_OFFSET + right_shake, right_box_y-SELECTED_OFFSET, TILE_SIZE, TILE_SIZE, UI_BG_COLOR);
                draw_rectangle_lines(
                    right_box_x-3.0-SELECTED_OFFSET + right_shake - offset,
                    right_box_y-3.0-SELECTED_OFFSET - offset,
                    scaled_size,
                    scaled_size,
                    cursor_thickness,
                    cursor_color
                );
            } else {
                draw_rectangle(right_box_x-2.0 + right_shake, right_box_y-2.0, TILE_SIZE+4.0, TILE_SIZE+4.0, UI_BG_COLOR);
            }

            let right_offset = if let UIFocus::StorageRight = input_state.ui_focus {
                SELECTED_OFFSET
            } else {
                0.0
            };
            let right_points = [
                Vec2::new(right_box_x + TILE_SIZE/2.0 + NAV_ARROW_SIZE - 4.0 - right_offset + right_shake, right_box_y + TILE_SIZE/2.0 - right_offset),
                Vec2::new(right_box_x + TILE_SIZE/2.0 - 4.0 - right_offset + right_shake, right_box_y + TILE_SIZE/2.0 - NAV_ARROW_SIZE - right_offset),
                Vec2::new(right_box_x + TILE_SIZE/2.0 - 4.0 - right_offset + right_shake, right_box_y + TILE_SIZE/2.0 + NAV_ARROW_SIZE - right_offset),
            ];
            let right_color = if state.selected < state.media.len() - 1 {
                WHITE
            } else {
                Color { r: 0.3, g: 0.3, b: 0.3, a: 1.0 } // Dark gray when disabled
            };
            draw_triangle(right_points[0], right_points[1], right_points[2], right_color);
            draw_triangle_lines(right_points[0], right_points[1], right_points[2], NAV_ARROW_OUTLINE, BLACK);

            // Draw storage info text
            text(&ctx, &state.media[state.selected].id, STORAGE_INFO_X + 2.0, STORAGE_INFO_Y + 17.0);
            text(&ctx, &format!("{} MB Free", state.media[state.selected].free as f32), STORAGE_INFO_X + 2.0, STORAGE_INFO_Y + 33.0);
        }
    }

    // Draw highlight box for save info
    draw_rectangle(16.0, 309.0, SCREEN_WIDTH as f32 - 32.0, 40.0, UI_BG_COLOR);
    draw_rectangle_lines(12.0, 305.0, SCREEN_WIDTH as f32 - 24.0, 48.0, 4.0, UI_BG_COLOR_DARK);

    let memory_index = get_memory_index(selected_memory, scroll_offset);
    if input_state.ui_focus == UIFocus::Grid {
        if let Some(selected_mem) = memories.get(memory_index) {
            let desc = match selected_mem.name.clone() {
                Some(name) => name,
                None => selected_mem.id.clone(),
            };

            let playtime = get_game_playtime(selected_mem, playtime_cache);
            let size = get_game_size(selected_mem, size_cache);
            text(&ctx, &desc, 19.0, 327.0);
            text(&ctx, &format!("{:.1} MB | {:.1} H", size, playtime), 19.0, 345.0);
        }
    }

    // Draw scroll indicators last so they appear on top
    const SCROLL_INDICATOR_SIZE: f32 = 8.0;  // Size from center to edge
    const SCROLL_INDICATOR_DISTANCE_TOP: f32 = -13.0;  // Distance from grid edge
    const SCROLL_INDICATOR_DISTANCE_BOTTOM: f32 = 4.0;  // Distance from grid edge
    const SCROLL_INDICATOR_OUTLINE: f32 = 1.0;  // Outline thickness

    if scroll_offset > 0 {
        // Up arrow (pointing up)
        let points = [
            Vec2::new(SCREEN_WIDTH as f32 / 2.0, GRID_OFFSET - SCROLL_INDICATOR_DISTANCE_TOP - SCROLL_INDICATOR_SIZE),
            Vec2::new(SCREEN_WIDTH as f32 / 2.0 - SCROLL_INDICATOR_SIZE, GRID_OFFSET - SCROLL_INDICATOR_DISTANCE_TOP),
            Vec2::new(SCREEN_WIDTH as f32 / 2.0 + SCROLL_INDICATOR_SIZE, GRID_OFFSET - SCROLL_INDICATOR_DISTANCE_TOP),
        ];
        draw_triangle(points[0], points[1], points[2], WHITE);
        draw_triangle_lines(points[0], points[1], points[2], SCROLL_INDICATOR_OUTLINE, BLACK);
    }

    let next_row_start = get_memory_index(GRID_WIDTH * GRID_HEIGHT, scroll_offset);
    if next_row_start < memories.len() {
        // Down arrow (pointing down)
        let grid_bottom = GRID_OFFSET + GRID_HEIGHT as f32 * (TILE_SIZE + PADDING);
        let points = [
            Vec2::new(SCREEN_WIDTH as f32 / 2.0, grid_bottom + SCROLL_INDICATOR_DISTANCE_BOTTOM + SCROLL_INDICATOR_SIZE),
            Vec2::new(SCREEN_WIDTH as f32 / 2.0 - SCROLL_INDICATOR_SIZE, grid_bottom + SCROLL_INDICATOR_DISTANCE_BOTTOM),
            Vec2::new(SCREEN_WIDTH as f32 / 2.0 + SCROLL_INDICATOR_SIZE, grid_bottom + SCROLL_INDICATOR_DISTANCE_BOTTOM),
        ];
        draw_triangle(points[0], points[1], points[2], WHITE);
        draw_triangle_lines(points[0], points[1], points[2], SCROLL_INDICATOR_OUTLINE, BLACK);
    }
}

fn render_dialog(
    ctx: &DrawContext,
    dialog: &Dialog,
    memories: &Vec<Memory>,
    selected_memory: usize,
    icon_cache: &HashMap<String, Texture2D>,
    copy_op_state: &Arc<Mutex<CopyOperationState>>,
    placeholder: &Texture2D,
    scroll_offset: usize,
    animation_state: &AnimationState,
    playtime_cache: &mut PlaytimeCache,
    size_cache: &mut SizeCache,
) {
    let (copy_progress, copy_running) = {
        if let Ok(state) = copy_op_state.lock() {
            (state.progress, state.running)
        } else {
            (0, false)
        }
    };

    // Only show dialog background and content when animation is complete
    if animation_state.dialog_transition_progress >= 1.0 {
        draw_rectangle(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32, UI_BG_COLOR_DIALOG);
    }

    // draw game icon and name
    let memory_index = get_memory_index(selected_memory, scroll_offset);
    if let Some(mem) = memories.get(memory_index) {
        let icon = match icon_cache.get(&mem.id) {
            Some(icon) => icon,
            None => &placeholder,
        };

        let params = DrawTextureParams {
            dest_size: Some(Vec2 {x: TILE_SIZE, y: TILE_SIZE }),
            source: Some(Rect { x: 0.0, y: 0.0, h: icon.height(), w: icon.width() }),
            rotation: 0.0,
            flip_x: false,
            flip_y: false,
            pivot: None
        };

        // Use transition position for icon
        let icon_pos = animation_state.get_dialog_transition_pos();
        draw_texture_ex(&icon, icon_pos.x, icon_pos.y, WHITE, params);

        // Only show text when animation is complete
        if animation_state.dialog_transition_progress >= 1.0 {
            let desc = match mem.name.clone() {
                Some(name) => name,
                None => mem.id.clone(),
            };

            let playtime = get_game_playtime(mem, playtime_cache);
            let size = get_game_size(mem, size_cache);
            text(&ctx, &desc, TILE_SIZE*2.0, TILE_SIZE-1.0);
            text(&ctx, &format!("{:.1} MB | {:.1} H", size, playtime), TILE_SIZE*2.0, TILE_SIZE*1.5+1.0);
        }
    };

    if copy_running {
        draw_rectangle_lines(
            (FONT_SIZE*3) as f32,
            SCREEN_HEIGHT as f32 / 2.0,
            (SCREEN_WIDTH as u16 - FONT_SIZE*6) as f32,
            1.2*FONT_SIZE as f32,
            4.0,
            Color {r: 1.0, g: 1.0, b: 1.0, a: 1.0 }
        );
        draw_rectangle(
            (FONT_SIZE*3) as f32 + 0.2*FONT_SIZE as f32,
            SCREEN_HEIGHT as f32 / 2.0 + 0.2*FONT_SIZE as f32,
            ((SCREEN_WIDTH as u16 - FONT_SIZE*6) as f32 - 0.4*FONT_SIZE as f32) * (copy_progress as f32 / 100.0),
            0.8*FONT_SIZE as f32,
            Color {r: 1.0, g: 1.0, b: 1.0, a: 1.0 }
        );
    } else if animation_state.dialog_transition_progress >= 1.0 {
        if let Some(desc) = dialog.desc.clone() {
            text(&ctx, &desc, (SCREEN_WIDTH as f32 - measure_text(&desc, Some(&ctx.font), FONT_SIZE, 1.0).width) / 2.0, (FONT_SIZE*7) as f32);
        }

        // Find the longest option text for centering
        let longest_option = dialog.options.iter()
            .map(|opt| opt.text.len())
            .max()
            .unwrap_or(0);

        // Calculate the width of the longest option in pixels
        let longest_width = measure_text(&dialog.options.iter()
            .find(|opt| opt.text.len() == longest_option)
            .map(|opt| opt.text.to_uppercase())
            .unwrap_or_default(),
            Some(&ctx.font),
            FONT_SIZE,
            1.0).width;

        // Calculate the starting X position to center all options
        let options_start_x = (SCREEN_WIDTH as f32 - longest_width) / 2.0;

        // Add padding to the selection rectangle
        const SELECTION_PADDING_X: f32 = 16.0;  // Padding on each side
        const SELECTION_PADDING_Y: f32 = 4.0;   // Padding on top and bottom

        for (i, option) in dialog.options.iter().enumerate() {
            let y_pos = (FONT_SIZE*10 + FONT_SIZE*2*(i as u16)) as f32;
            let shake_offset = if option.disabled {
                animation_state.calculate_shake_offset(ShakeTarget::Dialog)
            } else {
                0.0
            };
            if option.disabled {
                text_disabled(&ctx, &option.text, options_start_x + shake_offset, y_pos);
            } else {
                text(&ctx, &option.text, options_start_x, y_pos);
            }
        }

        // Draw selection rectangle with padding
        let selection_y = (FONT_SIZE*9 + FONT_SIZE*2*(dialog.selection as u16)) as f32;
        let selected_option = &dialog.options[dialog.selection];
        let selection_shake = if selected_option.disabled {
            animation_state.calculate_shake_offset(ShakeTarget::Dialog)
        } else {
            0.0
        };

        let cursor_color = animation_state.get_cursor_color();
        let cursor_scale = animation_state.get_cursor_scale();
        let base_width = longest_width + (SELECTION_PADDING_X * 2.0);
        let base_height = 1.2*FONT_SIZE as f32 + (SELECTION_PADDING_Y * 2.0);
        let scaled_width = base_width * cursor_scale;
        let scaled_height = base_height * cursor_scale;
        let offset_x = (scaled_width - base_width) / 2.0;
        let offset_y = (scaled_height - base_height) / 2.0;

        draw_rectangle_lines(
            options_start_x - SELECTION_PADDING_X + selection_shake - offset_x,
            selection_y - SELECTION_PADDING_Y - offset_y,
            scaled_width,
            scaled_height,
            4.0,
            cursor_color
        );
    }
}

fn create_confirm_delete_dialog() -> Dialog {
    Dialog {
        id: "confirm_delete".to_string(),
        desc: Some("PERMANENTLY DELETE THIS SAVE DATA?".to_string()),
        options: vec![
            DialogOption {
                text: "DELETE".to_string(),
                value: "DELETE".to_string(),
                disabled: false,
            },
            DialogOption {
                text: "CANCEL".to_string(),
                value: "CANCEL".to_string(),
                disabled: false,
            }
        ],
        selection: 1,
    }
}

fn create_copy_storage_dialog(storage_state: &Arc<Mutex<StorageMediaState>>) -> Dialog {
    let mut options = Vec::new();
    if let Ok(state) = storage_state.lock() {
        for drive in state.media.iter() {
            if drive.id == state.media[state.selected].id {
                continue;
            }
            options.push(DialogOption {
                text: format!("{} ({} MB Free)", drive.id.clone(), drive.free),
                value: drive.id.clone(),
                disabled: false,
            });
        }
    }
    options.push(DialogOption {
        text: "CANCEL".to_string(),
        value: "CANCEL".to_string(),
        disabled: false,
    });

    Dialog {
        id: "copy_storage_select".to_string(),
        desc: Some("WHERE TO COPY THIS SAVE DATA?".to_string()),
        options,
        selection: 0,
    }
}

fn create_main_dialog(storage_state: &Arc<Mutex<StorageMediaState>>) -> Dialog {
    let has_external_devices = if let Ok(state) = storage_state.lock() {
        state.media.len() > 1
    } else {
        false
    };

    let options = vec![
        DialogOption {
            text: "COPY".to_string(),
            value: "COPY".to_string(),
            disabled: !has_external_devices,
        },
        DialogOption {
            text: "DELETE".to_string(),
            value: "DELETE".to_string(),
            disabled: false,
        },
        DialogOption {
            text: "CANCEL".to_string(),
            value: "CANCEL".to_string(),
            disabled: false,
        },
    ];

    Dialog {
        id: "main".to_string(),
        desc: None,
        options,
        selection: 0,
    }
}

async fn check_save_exists(memory: &Memory, target_media: &StorageMedia, icon_cache: &mut HashMap<String, Texture2D>, icon_queue: &mut Vec<(String, String)>) -> bool {
    let target_memories = load_memories(target_media, icon_cache, icon_queue).await;
    target_memories.iter().any(|m| m.id == memory.id)
}

fn create_save_exists_dialog() -> Dialog {
    Dialog {
        id: "save_exists".to_string(),
        desc: Some("THIS SAVE DATA ALREADY EXISTS AT THE SELECTED DESTINATION".to_string()),
        options: vec![
            DialogOption {
                text: "OK".to_string(),
                value: "OK".to_string(),
                disabled: false,
            }
        ],
        selection: 0,
    }
}

fn create_error_dialog(message: String) -> Dialog {
    Dialog {
        id: "error".to_string(),
        desc: Some(message),
        options: vec![
            DialogOption {
                text: "OK".to_string(),
                value: "OK".to_string(),
                disabled: false,
            }
        ],
        selection: 0,
    }
}

#[derive(Clone, Debug, PartialEq)]
enum DialogState {
    None,
    Opening,
    Open,
    Closing,
}

fn render_main_menu(
    ctx: &DrawContext,
    menu_options: &[&str],
    selected_option: usize,
    play_option_enabled: bool,
    animation_state: &AnimationState,
    logo: &Texture2D,
) {
    const MENU_START_Y: f32 = 120.0;
    const MENU_OPTION_HEIGHT: f32 = 40.0;
    const MENU_PADDING: f32 = 16.0;

    // Draw background
    draw_rectangle(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32, UI_BG_COLOR);

    // Draw menu options
    for (i, option) in menu_options.iter().enumerate() {
        let y_pos = MENU_START_Y + (i as f32 * MENU_OPTION_HEIGHT);

        // Draw selected option highlight
        if i == selected_option {
            let cursor_color = animation_state.get_cursor_color();
            let cursor_scale = animation_state.get_cursor_scale();
            let base_width = measure_text(option, Some(&ctx.font), FONT_SIZE, 1.0).width + (MENU_PADDING * 2.0);
            let base_height = FONT_SIZE as f32 + (MENU_PADDING * 2.0);
            let scaled_width = base_width * cursor_scale;
            let scaled_height = base_height * cursor_scale;
            let offset_x = (scaled_width - base_width) / 2.0;
            let offset_y = (scaled_height - base_height) / 2.0;
            let mut x_pos = (SCREEN_WIDTH as f32 - base_width) / 2.0;

            // Apply shake effect to selection highlight for disabled play option
            if i == 1 && !play_option_enabled {
                let shake_offset = animation_state.calculate_shake_offset(ShakeTarget::PlayOption);
                x_pos += shake_offset;
            }

            draw_rectangle_lines(x_pos - offset_x, y_pos - 7.0 - offset_y, scaled_width, scaled_height/1.5, 4.0, cursor_color);
        }

        // Draw text
        let text_width = measure_text(option, Some(&ctx.font), FONT_SIZE, 1.0).width;
        let mut x_pos = (SCREEN_WIDTH as f32 - text_width) / 2.0;
        let y_pos_text = y_pos + MENU_PADDING;

        // Apply shake effect to disabled play option when selected
        if i == 1 && !play_option_enabled {
            let shake_offset = animation_state.calculate_shake_offset(ShakeTarget::PlayOption);
            x_pos += shake_offset;
        }

        if i == 1 && !play_option_enabled {
            text_disabled(&ctx, option, x_pos, y_pos_text);
        } else {
            text(&ctx, option, x_pos, y_pos_text);
        }
    }

    // Draw logo and version number
    draw_texture(logo, (SCREEN_WIDTH as f32 - 166.0)/2.0, 30.0, WHITE);
    text(&ctx, "V2026.0", SCREEN_WIDTH as f32 - 90.0, SCREEN_HEIGHT as f32 - 20.0);
}

#[macroquad::main(window_conf)]
async fn main() {
    let mut dialogs: Vec<Dialog> = Vec::new();
    let mut dialog_state = DialogState::None;
    let font = load_ttf_font_from_bytes(include_bytes!("../november.ttf")).unwrap();
    let background = Texture2D::from_file_with_format(include_bytes!("../background.png"), Some(ImageFormat::Png));
    let logo = Texture2D::from_file_with_format(include_bytes!("../logo.png"), Some(ImageFormat::Png));
    let placeholder = Texture2D::from_file_with_format(include_bytes!("../placeholder.png"), Some(ImageFormat::Png));
    let mut icon_cache: HashMap<String, Texture2D> = HashMap::new();
    let mut icon_queue: Vec<(String, String)> = Vec::new();
    let mut playtime_cache: PlaytimeCache = HashMap::new();
    let mut size_cache: SizeCache = HashMap::new();
    let mut scroll_offset = 0;

    let ctx : DrawContext = DrawContext {
        font: font,
    };

    // Initialize sound effects
    let sound_effects = SoundEffects::new().await;

    // Initialize gamepad support
    let mut gilrs = Gilrs::new().unwrap();
    let mut input_state = InputState::new();
    let mut animation_state = AnimationState::new();

    // Screen state
    const MAIN_MENU_OPTIONS: [&str; 2] = ["DATA", "PLAY"];
    let mut current_screen = Screen::MainMenu;
    let mut main_menu_selection: usize = 0;
    let mut play_option_enabled: bool = false;

    // Fade state
    let mut fade_start_time: Option<f64> = None;
    const FADE_DURATION: f64 = 1.0; // 1 second fade
    const FADE_LINGER_DURATION: f64 = 0.5; // 0.5 seconds to linger on black screen

    // Create thread-safe cart connection status
    let cart_connected = Arc::new(AtomicBool::new(false));
    let cart_check_thread_running = Arc::new(AtomicBool::new(false));

    // Spawn background thread for cart connection detection (only active during main menu)
    let cart_connected_clone = cart_connected.clone();
    let cart_check_thread_running_clone = cart_check_thread_running.clone();
    thread::spawn(move || {
        while cart_check_thread_running_clone.load(Ordering::Relaxed) {
            let is_connected = save::is_cart_connected();
            cart_connected_clone.store(is_connected, Ordering::Relaxed);
            thread::sleep(time::Duration::from_secs(1));
        }
    });

    // Create thread-safe storage media state
    let storage_state = Arc::new(Mutex::new(StorageMediaState::new()));

    // Initialize storage media list
    if let Ok(mut state) = storage_state.lock() {
        state.update_media();
    };

    // Spawn background thread for storage media detection
    let thread_storage_state = storage_state.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(time::Duration::from_secs(1));
            if let Ok(mut state) = thread_storage_state.lock() {
                state.update_media();
            }
        }
    });

    let mut memories = Vec::new();
    let mut selected_memory = 0;

    let copy_op_state = Arc::new(Mutex::new(CopyOperationState {
        progress: 0,
        running: false,
        should_clear_dialogs: false,
        error_message: None,
    }));


    let mut bgx = 0.0;

    let color_targets: [Color; 6] = [
        Color { r: 1.0, g: 0.5, b: 0.5, a: 1.0 },
        Color { r: 1.0, g: 1.0, b: 0.5, a: 1.0 },
        Color { r: 0.5, g: 1.0, b: 0.5, a: 1.0 },
        Color { r: 0.5, g: 1.0, b: 1.0, a: 1.0 },
        Color { r: 0.5, g: 0.5, b: 1.0, a: 1.0 },
        Color { r: 1.0, g: 0.5, b: 1.0, a: 1.0 },
    ];

    let mut bg_color = color_targets[0].clone();
    let mut tg_color = color_targets[1].clone();

    let mut target = 1;

    const DELTA: f32 = 0.0001;

    loop {
        draw_texture(&background, bgx-(SCREEN_WIDTH as f32), 0.0, bg_color);
        draw_texture(&background, bgx, 0.0, bg_color);
        bgx = (bgx + 0.1) % (SCREEN_WIDTH as f32);

        if bg_color.r < tg_color.r {
            bg_color.r += DELTA;
        } else if bg_color.r > tg_color.r {
            bg_color.r -= DELTA;
        }

        if bg_color.g < tg_color.g {
            bg_color.g += DELTA;
        } else if bg_color.g > tg_color.g {
            bg_color.g -= DELTA;
        }

        if bg_color.b < tg_color.b {
            bg_color.b += DELTA;
        } else if bg_color.b > tg_color.b {
            bg_color.b -= DELTA;
        }

        if (bg_color.r - tg_color.r).abs() < 0.01 && (bg_color.g - tg_color.g).abs() < 0.01 && (bg_color.b - tg_color.b).abs() < 0.01 {
            target = (target + 1) % 6;
            tg_color = color_targets[target].clone();
        }

        let mut action_dialog_id = String::new();
        let mut action_option_value = String::new();

        // Update input state from both keyboard and controller
        input_state.update_keyboard();
        input_state.update_controller(&mut gilrs);

        // Update animations
        animation_state.update_shake(get_frame_time());
        animation_state.update_cursor_animation(get_frame_time());
        animation_state.update_dialog_transition(get_frame_time());

        // Manage cart check thread based on current screen
        let should_thread_run = current_screen == Screen::MainMenu;
        let thread_is_running = cart_check_thread_running.load(Ordering::Relaxed);

        if should_thread_run && !thread_is_running {
            // Entered main menu, start cart check thread
            cart_check_thread_running.store(true, Ordering::Relaxed);
            let cart_connected_clone = cart_connected.clone();
            let cart_check_thread_running_clone = cart_check_thread_running.clone();
            thread::spawn(move || {
                while cart_check_thread_running_clone.load(Ordering::Relaxed) {
                    let is_connected = save::is_cart_connected();
                    cart_connected_clone.store(is_connected, Ordering::Relaxed);
                    thread::sleep(time::Duration::from_secs(1));
                }
            });
        } else if !should_thread_run && thread_is_running {
            // Left main menu, stop cart check thread
            cart_check_thread_running.store(false, Ordering::Relaxed);
        }

        // Update dialog state based on animation
        if animation_state.dialog_transition_time <= 0.0 {
            match dialog_state {
                DialogState::Opening => {
                    dialog_state = DialogState::Open;
                },
                DialogState::Closing => {
                    dialog_state = DialogState::None;
                    dialogs.clear();
                },
                _ => {}
            }
        }

        // Handle screen-specific rendering and input
        match current_screen {
            Screen::FadingOut => {
                // During fade, only render, don't process input
                // Render the current background and UI elements first
                render_main_menu(&ctx, &MAIN_MENU_OPTIONS, main_menu_selection, play_option_enabled, &animation_state, &logo);

                // Calculate fade progress
                if let Some(start_time) = fade_start_time {
                    let elapsed = get_time() - start_time;
                    let fade_progress = (elapsed / FADE_DURATION).min(1.0);

                    // Draw fade overlay
                    let alpha = fade_progress as f32;
                    draw_rectangle(0.0, 0.0, SCREEN_WIDTH as f32, SCREEN_HEIGHT as f32,
                        Color { r: 0.0, g: 0.0, b: 0.0, a: alpha });

                    // If fade is complete, wait for linger duration then exit
                    if fade_progress >= 1.0 {
                        let total_elapsed = elapsed - FADE_DURATION;
                        if total_elapsed >= FADE_LINGER_DURATION {
                            process::exit(0);
                        }
                    }
                }
            },
            Screen::MainMenu => {
                // Update play option enabled status based on cart connection
                play_option_enabled = cart_connected.load(Ordering::Relaxed);

                render_main_menu(&ctx, &MAIN_MENU_OPTIONS, main_menu_selection, play_option_enabled, &animation_state, &logo);

                // Handle main menu navigation
                if input_state.up {
                    if main_menu_selection == 0 {
                        main_menu_selection = MAIN_MENU_OPTIONS.len() - 1;
                    } else {
                        main_menu_selection = (main_menu_selection - 1) % MAIN_MENU_OPTIONS.len();
                    }
                    animation_state.trigger_transition();
                    sound_effects.play_cursor_move();
                }
                if input_state.down {
                    main_menu_selection = (main_menu_selection + 1) % MAIN_MENU_OPTIONS.len();
                    animation_state.trigger_transition();
                    sound_effects.play_cursor_move();
                }
                if input_state.select {
                    match main_menu_selection {
                        0 => {
                            current_screen = Screen::SaveData;
                            input_state.ui_focus = UIFocus::Grid;
                            sound_effects.play_select();
                        },
                        1 => {
                            if play_option_enabled {
                                sound_effects.play_select();
                                // Create restart session sentinel file and start fade
                                let state_dir = Path::new(".local/share/kazeta/state");
                                if state_dir.exists() {
                                    let sentinel_path = state_dir.join(".RESTART_SESSION_SENTINEL");
                                    if let Err(_) = fs::File::create(&sentinel_path) {
                                        // If we can't create the file, just continue
                                        // Don't show error to user
                                    }
                                }
                                // Start fade to black
                                fade_start_time = Some(get_time());
                                current_screen = Screen::FadingOut;
                            } else {
                                sound_effects.play_reject();
                                animation_state.trigger_play_option_shake();
                            }
                        },
                        _ => {}
                    }
                }
            },
            Screen::SaveData => {
                // Check if memories need to be refreshed due to storage media changes
                if let Ok(mut state) = storage_state.lock() {
                    if state.needs_memory_refresh {
                        if !state.media.is_empty() {
                            memories = load_memories(&state.media[state.selected], &mut icon_cache, &mut icon_queue).await;
                        } else {
                            memories = Vec::new();
                        }
                        state.needs_memory_refresh = false;
                        dialogs.clear();
                    }
                }

                match dialog_state {
                    DialogState::None => {
                        render_data_view(&ctx, selected_memory, &memories, &icon_cache, &storage_state, &placeholder, scroll_offset, &mut input_state, &mut animation_state, &mut playtime_cache, &mut size_cache);

                        // Handle back navigation
                        if input_state.back {
                           current_screen = Screen::MainMenu;
                           sound_effects.play_back();
                        }

                        // Handle storage media switching with tab/bumpers regardless of focus
                        if input_state.cycle || input_state.next || input_state.prev {
                            if let Ok(mut state) = storage_state.lock() {
                                if input_state.cycle {
                                    if state.media.len() > 1 {
                                        // Cycle wraps around
                                        state.selected = (state.selected + 1) % state.media.len();
                                        memories = load_memories(&state.media[state.selected], &mut icon_cache, &mut icon_queue).await;
                                        scroll_offset = 0;
                                        sound_effects.play_select();
                                    }
                                } else if input_state.next {
                                    // Next stops at end
                                    if state.selected < state.media.len() - 1 {
                                        state.selected += 1;
                                        memories = load_memories(&state.media[state.selected], &mut icon_cache, &mut icon_queue).await;
                                        scroll_offset = 0;
                                        sound_effects.play_select();
                                    } else {
                                        animation_state.trigger_shake(false); // Shake right arrow when can't go next
                                        sound_effects.play_reject();
                                    }
                                } else if input_state.prev {
                                    // Prev stops at beginning
                                    if state.selected > 0 {
                                        state.selected -= 1;
                                        memories = load_memories(&state.media[state.selected], &mut icon_cache, &mut icon_queue).await;
                                        scroll_offset = 0;
                                        sound_effects.play_select();
                                    } else {
                                        animation_state.trigger_shake(true); // Shake left arrow when can't go prev
                                        sound_effects.play_reject();
                                    }
                                }
                            }
                        }

                        match input_state.ui_focus {
                            UIFocus::Grid => {
                                if input_state.select {
                                    let memory_index = get_memory_index(selected_memory, scroll_offset);
                                    if let Some(_) = memories.get(memory_index) {
                                        let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                                        animation_state.trigger_dialog_transition(grid_pos, dialog_pos);
                                        dialogs.push(create_main_dialog(&storage_state));
                                        dialog_state = DialogState::Opening;
                                        sound_effects.play_select();
                                    }
                                }
                                if input_state.right && selected_memory < GRID_WIDTH * GRID_HEIGHT - 1 {
                                    selected_memory += 1;
                                    animation_state.trigger_transition();
                                    sound_effects.play_cursor_move();
                                }
                                if input_state.left && selected_memory >= 1 {
                                    selected_memory -= 1;
                                    animation_state.trigger_transition();
                                    sound_effects.play_cursor_move();
                                }
                                if input_state.down {
                                    if selected_memory < GRID_WIDTH * GRID_HEIGHT - GRID_WIDTH {
                                        selected_memory += GRID_WIDTH;
                                        animation_state.trigger_transition();
                                        sound_effects.play_cursor_move();
                                    } else {
                                        // Check if there are any saves in the next row
                                        let next_row_start = get_memory_index(GRID_WIDTH * GRID_HEIGHT, scroll_offset);
                                        if next_row_start < memories.len() {
                                            scroll_offset += 1;
                                            animation_state.trigger_transition();
                                            sound_effects.play_cursor_move();
                                        }
                                    }
                                }
                                if input_state.up {
                                    if selected_memory >= GRID_WIDTH {
                                        selected_memory -= GRID_WIDTH;
                                        animation_state.trigger_transition();
                                        sound_effects.play_cursor_move();
                                    } else if scroll_offset > 0 {
                                        scroll_offset -= 1;
                                        animation_state.trigger_transition();
                                        sound_effects.play_cursor_move();
                                    } else {
                                        // Allow moving to storage navigation from leftmost or rightmost column
                                        if selected_memory % GRID_WIDTH == 0 {
                                            input_state.ui_focus = UIFocus::StorageLeft;
                                            animation_state.trigger_transition();
                                            sound_effects.play_cursor_move();
                                        } else if selected_memory % GRID_WIDTH == GRID_WIDTH - 1 {
                                            input_state.ui_focus = UIFocus::StorageRight;
                                            animation_state.trigger_transition();
                                            sound_effects.play_cursor_move();
                                        }
                                    }
                                }
                            },
                            UIFocus::StorageLeft => {
                                if input_state.right {
                                    input_state.ui_focus = UIFocus::StorageRight;
                                    animation_state.trigger_transition();
                                    sound_effects.play_cursor_move();
                                }
                                if input_state.down {
                                    input_state.ui_focus = UIFocus::Grid;
                                    selected_memory = 0; // Move to leftmost grid position
                                    animation_state.trigger_transition();
                                    sound_effects.play_cursor_move();
                                }
                                if input_state.select {
                                    if let Ok(mut state) = storage_state.lock() {
                                        if state.selected > 0 {
                                            state.selected -= 1;
                                            memories = load_memories(&state.media[state.selected], &mut icon_cache, &mut icon_queue).await;
                                            scroll_offset = 0;
                                            sound_effects.play_select();
                                        } else {
                                            animation_state.trigger_shake(true);
                                            sound_effects.play_reject();
                                        }
                                    }
                                }
                            },
                            UIFocus::StorageRight => {
                                if input_state.left {
                                    input_state.ui_focus = UIFocus::StorageLeft;
                                    animation_state.trigger_transition();
                                    sound_effects.play_cursor_move();
                                }
                                if input_state.down {
                                    input_state.ui_focus = UIFocus::Grid;
                                    selected_memory = GRID_WIDTH - 1; // Move to rightmost grid position
                                    animation_state.trigger_transition();
                                    sound_effects.play_cursor_move();
                                }
                                if input_state.select {
                                    if let Ok(mut state) = storage_state.lock() {
                                        if state.selected < state.media.len() - 1 {
                                            state.selected += 1;
                                            memories = load_memories(&state.media[state.selected], &mut icon_cache, &mut icon_queue).await;
                                            scroll_offset = 0;
                                            sound_effects.play_select();
                                        } else {
                                            animation_state.trigger_shake(false);
                                            sound_effects.play_reject();
                                        }
                                    }
                                }
                            },
                        }
                    },
                    DialogState::Opening => {
                        // During opening, only render the main view and the transitioning icon
                        render_data_view(&ctx, selected_memory, &memories, &icon_cache, &storage_state, &placeholder, scroll_offset, &mut input_state, &mut animation_state, &mut playtime_cache, &mut size_cache);
                        // Only render the icon during transition
                        let memory_index = get_memory_index(selected_memory, scroll_offset);
                        if let Some(mem) = memories.get(memory_index) {
                            let icon = match icon_cache.get(&mem.id) {
                                Some(icon) => icon,
                                None => &placeholder,
                            };

                            let params = DrawTextureParams {
                                dest_size: Some(Vec2 {x: TILE_SIZE, y: TILE_SIZE }),
                                source: Some(Rect { x: 0.0, y: 0.0, h: icon.height(), w: icon.width() }),
                                rotation: 0.0,
                                flip_x: false,
                                flip_y: false,
                                pivot: None
                            };

                            let icon_pos = animation_state.get_dialog_transition_pos();
                            draw_texture_ex(&icon, icon_pos.x, icon_pos.y, WHITE, params);
                        }
                    },
                    DialogState::Open => {
                        // When dialog is fully open, only render the dialog
                        if let Some(dialog) = dialogs.last_mut() {
                            render_dialog(&ctx, dialog, &memories, selected_memory, &icon_cache, &copy_op_state, &placeholder, scroll_offset, &animation_state, &mut playtime_cache, &mut size_cache);

                            let mut selection: i32 = dialog.selection as i32 + dialog.options.len() as i32;
                            if input_state.up {
                                selection -= 1;
                                animation_state.trigger_transition();
                                sound_effects.play_cursor_move();
                            }

                            if input_state.down {
                                selection += 1;
                                animation_state.trigger_transition();
                                sound_effects.play_cursor_move();
                            }

                            let mut cancel = false;
                            if input_state.back {
                                cancel = true;
                            }

                            let next_selection = selection as usize % dialog.options.len();
                            if next_selection != dialog.selection {
                                // Store the new selection to apply after we're done with the immutable borrow
                                let new_selection = next_selection;
                                dialog.selection = new_selection;
                            } else {
                                // We need to handle the select input
                                if input_state.select {
                                    let selected_option = &dialog.options[dialog.selection];
                                    if !selected_option.disabled {
                                        action_dialog_id = dialog.id.clone();
                                        action_option_value = selected_option.value.clone();

                                        if selected_option.value == "CANCEL" || selected_option.value == "OK" {
                                            cancel = true;
                                        } else {
                                            sound_effects.play_select();
                                        }
                                    } else {
                                        animation_state.trigger_dialog_shake();
                                        sound_effects.play_reject();
                                    }
                                }
                            }

                            if cancel {
                                let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                                animation_state.trigger_dialog_transition(dialog_pos, grid_pos);
                                dialog_state = DialogState::Closing;
                                sound_effects.play_back();
                            }
                        }
                    },
                    DialogState::Closing => {
                        // During closing, render both views to show the icon returning
                        render_data_view(&ctx, selected_memory, &memories, &icon_cache, &storage_state, &placeholder, scroll_offset, &mut input_state, &mut animation_state, &mut playtime_cache, &mut size_cache);
                        // Only render the icon during transition
                        let memory_index = get_memory_index(selected_memory, scroll_offset);
                        if let Some(mem) = memories.get(memory_index) {
                            let icon = match icon_cache.get(&mem.id) {
                                Some(icon) => icon,
                                None => &placeholder,
                            };

                            let params = DrawTextureParams {
                                dest_size: Some(Vec2 {x: TILE_SIZE, y: TILE_SIZE }),
                                source: Some(Rect { x: 0.0, y: 0.0, h: icon.height(), w: icon.width() }),
                                rotation: 0.0,
                                flip_x: false,
                                flip_y: false,
                                pivot: None
                            };

                            let icon_pos = animation_state.get_dialog_transition_pos();
                            draw_texture_ex(&icon, icon_pos.x, icon_pos.y, WHITE, params);
                        }
                    }
                }
            },
        }

        // Handle dialog actions
        match (action_dialog_id.as_str(), action_option_value.as_str()) {
            ("main", "COPY") => {
                dialogs.push(create_copy_storage_dialog(&storage_state));
            },
            ("main", "DELETE") => {
                dialogs.push(create_confirm_delete_dialog());
            },
            ("main", "CANCEL") => {
                let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                animation_state.trigger_dialog_transition(dialog_pos, grid_pos);
                dialog_state = DialogState::Closing;
                sound_effects.play_back();
            },
            ("confirm_delete", "DELETE") => {
                if let Ok(mut state) = storage_state.lock() {
                    let memory_index = get_memory_index(selected_memory, scroll_offset);
                    if let Some(mem) = memories.get(memory_index) {
                        if let Err(e) = save::delete_save(&mem.id, &state.media[state.selected].id) {
                            dialogs.push(create_error_dialog(format!("ERROR: {}", e)));
                        } else {
                            state.needs_memory_refresh = true;
                            dialog_state = DialogState::None;
                            sound_effects.play_back();
                        }
                    }
                }
            },
            ("confirm_delete", "CANCEL") => {
                let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                animation_state.trigger_dialog_transition(dialog_pos, grid_pos);
                dialog_state = DialogState::Closing;
                sound_effects.play_back();
            },
            ("copy_storage_select", target_id) if target_id != "CANCEL" => {
                let memory_index = get_memory_index(selected_memory, scroll_offset);
                let mem = memories[memory_index].clone();
                let target_id = target_id.to_string();
                if let Ok(state) = storage_state.lock() {
                    let to_media = StorageMedia { id: target_id, free: 0 };

                    // Check if save already exists
                    if check_save_exists(&mem, &to_media, &mut icon_cache, &mut icon_queue).await {
                        dialogs.push(create_save_exists_dialog());
                    } else {
                        let thread_state = copy_op_state.clone();
                        let from_media = state.media[state.selected].clone();
                        thread::spawn(move || {
                            copy_memory(&mem, &from_media, &to_media, thread_state);
                        });
                    }
                }
            },
            ("copy_storage_select", "CANCEL") => {
                let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                animation_state.trigger_dialog_transition(dialog_pos, grid_pos);
                dialog_state = DialogState::Closing;
                sound_effects.play_back();
            },
            ("save_exists", "OK") => {
                let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                animation_state.trigger_dialog_transition(dialog_pos, grid_pos);
                dialog_state = DialogState::Closing;
                sound_effects.play_back();
            },
            ("error", "OK") => {
                let (grid_pos, dialog_pos) = calculate_icon_transition_positions(selected_memory);
                animation_state.trigger_dialog_transition(dialog_pos, grid_pos);
                dialog_state = DialogState::Closing;
                sound_effects.play_back();
            },
            _ => {}
        }

        if !icon_queue.is_empty() {
            let (cart_id, icon_path) = icon_queue.remove(0);
            let texture_future = load_texture(&icon_path);
            let texture_result = panic::catch_unwind(|| {
                futures::executor::block_on(texture_future)
            });

            if let Ok(Ok(texture)) = texture_result {
                icon_cache.insert(cart_id.clone(), texture);
            }
        }

        // Display any copy operation errors
        if let Ok(mut copy_state) = copy_op_state.lock() {
            if let Some(error_msg) = copy_state.error_message.take() {
                dialogs.push(create_error_dialog(error_msg));
                dialog_state = DialogState::Opening;
            }
            if copy_state.should_clear_dialogs {
                dialog_state = DialogState::Closing;
                copy_state.should_clear_dialogs = false;
            }
        }

        next_frame().await
    }
}
