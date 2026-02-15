#![windows_subsystem = "windows"]


use active_win_pos_rs::get_active_window;
use anyhow::{Context, Result};
use chrono::Utc;
use native_dialog::{MessageDialog, MessageType};
use rdev::{listen, Event, EventType};
use serde::Serialize;
use sha2::{Sha256, Digest};
use std::fs::{create_dir_all, OpenOptions, File};
use std::io::{Write, BufWriter};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
// use tao::platform::windows::EventLoopBuilderExtWindows; // Removed unused
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

const BATCH_SIZE: usize = 20;
const AUTO_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const SALT: &str = "GHOSTWRITER_SECURE_SALT_V1";

#[derive(Serialize, Clone)]
struct LogEvent {
    timestamp: i64,
    window_title: String,
    event_type: String,
}

#[derive(Serialize)]
struct Header {
    version: String,
    algorithm: String,
}

#[derive(Serialize)]
struct SignedSession {
    header: Header,
    payload: Vec<LogEvent>,
    signature: String,
}

fn get_session_file_path() -> Result<PathBuf> {
    let mut path = dirs::document_dir().context("Could not find Documents directory")?;
    path.push("Ghostwriter");
    create_dir_all(&path).context("Failed to create Ghostwriter directory")?;
    path.push("session.json");
    Ok(path)
}

fn get_session_file_path_timestamped() -> Result<PathBuf> {
    let mut path = dirs::document_dir().context("Could not find Documents directory")?;
    path.push("Ghostwriter");
    create_dir_all(&path).context("Failed to create Ghostwriter directory")?;
    let filename = format!("Session_{}.gw", Utc::now().timestamp());
    path.push(filename);
    Ok(path)
}

fn get_temp_file_path() -> Result<PathBuf> {
    let mut path = dirs::document_dir().context("Could not find Documents directory")?;
    path.push("Ghostwriter");
    create_dir_all(&path)?;
    path.push(".ghostwriter.tmp");
    Ok(path)
}


fn load_icon() -> tray_icon::Icon {
    // Generate a simple colored square icon (Red/Green logic later, just generic for now)
    // 32x32 RGBA
    let width = 32;
    let height = 32;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..height {
        for _ in 0..width {
            // Bright Red Color for better visibility
            rgba.push(255); // R
            rgba.push(0);   // G
            rgba.push(0);   // B
            rgba.push(255); // A
        }
    }
    tray_icon::Icon::from_rgba(rgba, width, height).expect("Failed to create icon")
}

fn main() -> Result<()> {
    // 0. Notify User of Startup (Crucial for silent tray apps)
    // We do this before the event loop
    let _ = MessageDialog::new()
        .set_type(MessageType::Info)
        .set_title("Ghostwriter CLI")
        .set_text("Ghostwriter is running in the System Tray (bottom-right of taskbar).\n\nLook for the Red Square icon.\nCheck the 'overflow' menu (^) if you don't see it.")
        .show_alert();

    // 1. Setup Architecture
    let event_loop = EventLoopBuilder::new().build();

    // 2. Setup Tray Menu
    // Item 1: Status (Disabled)
    let status_item = MenuItem::new("Status: Idle", false, None);
    
    // Item 2: Separator
    let separator = PredefinedMenuItem::separator();
    
    // Item 3: Start
    let start_item = MenuItem::new("Start Recording", true, None);
    
    // Item 4: Stop
    let stop_item = MenuItem::new("Stop Recording", true, None); // Initially enabled, but logic handles it
    
    // Item 5: Open Folder
    let open_folder_item = MenuItem::new("Open Logs Folder", true, None);
    
    // Item 6: Quit
    let quit_item = MenuItem::new("Quit", true, None);

    let menu = Menu::new();
    menu.append_items(&[
        &status_item,
        &separator,
        &start_item,
        &stop_item,
        &open_folder_item,
        &quit_item,
    ])?;

    // 3. Create Tray Icon
    let mut _tray_icon = Some(
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Ghostwriter CLI")
            .with_icon(load_icon())
            .build()
            .context("Failed to build tray icon")?,
    );

    // 4. Setup State and Channels
    let is_recording = Arc::new(AtomicBool::new(false)); // Default to NOT recording
    let shared_buffer = Arc::new(Mutex::new(Vec::new())); // Shared buffer for events
    let (tx, rx) = channel::<i64>();

    // 5. Spawn Worker Threads
    
    // Thread A: Sensor (rdev)
    // Listens to global events. Checks `is_recording`. If true, sends to channel.
    let is_recording_sensor = is_recording.clone();
    let tx_sensor = tx.clone();
    thread::spawn(move || {
        if let Err(error) = listen(move |event: Event| {
            if is_recording_sensor.load(Ordering::Relaxed) {
                match event.event_type {
                    EventType::KeyPress(_) => {
                        let ts = Utc::now().timestamp_millis();
                        let _ = tx_sensor.send(ts);
                    }
                    _ => {}
                }
            }
        }) {
            eprintln!("Error in sensor thread: {:?}", error);
        }
    });

    // Thread B: Writer (Processor)
    // Receives timestamps, adds window info, buffers, and writes.
    let buffer_writer = shared_buffer.clone();
    thread::spawn(move || {
        process_logs(rx, buffer_writer);
    });

    // 6. Run Event Loop (Main Thread)
    // We need to keep track of menu IDs to know what was clicked
    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();

    event_loop.run(move |_event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        // Handle Menu Events
        if let Ok(event) = menu_channel.try_recv() {
            if event.id == start_item.id() {
                is_recording.store(true, Ordering::SeqCst);
                let _ = status_item.set_text("Status: Recording");
                // Optional: Update icon to Green
            } else if event.id == stop_item.id() {
                is_recording.store(false, Ordering::SeqCst);
                let _ = status_item.set_text("Status: Idle");
                
                // Finalize and Seal
                let buffer_lock = shared_buffer.lock().unwrap();
                if !buffer_lock.is_empty() {
                    if let Ok(path) = get_session_file_path_timestamped() {
                         if let Err(e) = save_signed_session(&buffer_lock, &path) {
                             eprintln!("Failed to save signed session: {:?}", e);
                             let _ = MessageDialog::new()
                                .set_type(MessageType::Error)
                                .set_title("Save Error")
                                .set_text(&format!("Failed to save session: {}", e))
                                .show_alert();
                         }
                    }
                }
                // Clear the buffer after saving (or even if empty/failed, to reset state)
                drop(buffer_lock); // Unlock to allow clearing
                shared_buffer.lock().unwrap().clear();
                
                // Optional: Update icon to Red/Default
            } else if event.id == open_folder_item.id() {
                let path = match get_session_file_path() {
                    Ok(p) => p,
                    Err(_) => PathBuf::from("."),
                };
                // We want the parent directory
                let folder = path.parent().unwrap_or(&path).to_path_buf();
                let _ = open::that(folder);
            } else if event.id == quit_item.id() {
                 // Drop the icon to remove it from tray immediately
                _tray_icon = None;
                *control_flow = ControlFlow::Exit;
            }
        }

        // Handle Tray Events (e.g. double click)
        if let Ok(_event) = tray_channel.try_recv() {
            // Can handle tray icon clicks here if needed
        }
    });
}

fn process_logs(rx: Receiver<i64>, shared_buffer: Arc<Mutex<Vec<LogEvent>>>) {
    let mut last_auto_save = Instant::now();
    let temp_path = get_temp_file_path().unwrap_or_else(|_| PathBuf::from(".ghostwriter.tmp"));

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ts) => {
                let window_title = match get_active_window() {
                    Ok(window) => window.title,
                    Err(_) => "Unknown".to_string(),
                };

                let event = LogEvent {
                    timestamp: ts,
                    window_title,
                    event_type: "keypress".to_string(),
                };
                
                let mut buffer = shared_buffer.lock().unwrap();
                buffer.push(event);
            }
            Err(RecvTimeoutError::Timeout) => {
                // Continue to check auto-save
            }
            Err(RecvTimeoutError::Disconnected) => {
                break;
            }
        }

        // Auto-Save / Dump logic
        if last_auto_save.elapsed() >= AUTO_SAVE_INTERVAL {
            let buffer = shared_buffer.lock().unwrap();
            if !buffer.is_empty() {
                 if let Err(e) = dump_to_temp(&buffer, &temp_path) {
                     eprintln!("Auto-save failed: {:?}", e);
                 }
            }
            last_auto_save = Instant::now();
        }
    }
}

fn dump_to_temp(buffer: &[LogEvent], path: &PathBuf) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, buffer)?;
    Ok(())
}

fn save_signed_session(buffer: &[LogEvent], path: &PathBuf) -> Result<()> {
    // 1. Create Payload
    let payload_json = serde_json::to_string(buffer)?;
    
    // 2. Calculate Hash
    // Hash = SHA256( payload_json + SALT )
    let mut hasher = Sha256::new();
    hasher.update(&payload_json);
    hasher.update(SALT);
    let result = hasher.finalize();
    let signature = hex::encode(result);

    // 3. Create Signed Struct
    let session = SignedSession {
        header: Header {
            version: "1.0".to_string(),
            algorithm: "HMAC-SHA256".to_string(),
        },
        payload: buffer.to_vec(),
        signature,
    };

    // 4. Write to file
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, &session)?; // Pretty print for "official" look
    
    Ok(())
}
