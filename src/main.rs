use active_win_pos_rs::get_active_window;
use anyhow::{Context, Result};
use chrono::Utc;
use rdev::{listen, Event, EventType};
use serde::Serialize;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const BATCH_SIZE: usize = 20;
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Serialize)]
struct LogEvent {
    timestamp: i64, // Unix timestamp in milliseconds
    window_title: String,
    event_type: String,
}

fn get_session_file_path() -> Result<PathBuf> {
    let mut path = dirs::document_dir().context("Could not find Documents directory")?;
    path.push("Ghostwriter");
    create_dir_all(&path).context("Failed to create Ghostwriter directory")?;
    path.push("session.json");
    Ok(path)
}

fn main() -> Result<()> {
    // Graceful shutdown handling
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        println!("\nCtrl+C received. Exiting...");
        r.store(false, Ordering::SeqCst);
        // We might need to force exit if the listener thread is blocked, 
        // but for now we'll rely on the main loop checking 'running' 
        // or just let the OS cleanup since we are writing append-only.
        std::process::exit(0); 
    })
    .context("Error setting Ctrl-C handler")?;

    println!("Ghostwriter CLI started. Listening for key events...");

    let (tx, rx) = channel::<i64>();

    // Thread A: The Sensor
    // rdev::listen triggers a callback for every event.
    // Note: rdev::listen blocks the thread it runs on.
    thread::spawn(move || {
        if let Err(error) = listen(move |event: Event| {
            match event.event_type {
                EventType::KeyPress(_) => {
                    // Privacy: We explicitly ignore the key value.
                    // Just send the timestamp immediately.
                    let ts = Utc::now().timestamp_millis();
                    let _ = tx.send(ts); // Ignore send errors (receiver might be closed)
                }
                _ => {} // Ignore other events like Release or Mouse
            }
        }) {
            eprintln!("Error: {:?}", error);
        }
    });

    // Thread B: The Processor (Main Thread)
    let mut buffer: Vec<LogEvent> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();
    let session_path = get_session_file_path()?;

    println!("Logging into: {:?}", session_path);

    // We loop and check for messages. 
    // Since rx.recv() is blocking, we might want to use recv_timeout 
    // to strictly enforce the FLUSH_INTERVAL even if no keys are pressed.
    while running.load(Ordering::SeqCst) {
        // Wait for an event or timeout to flush
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ts) => {
                // Fetch Active Window Title immediately
                let window_title = match get_active_window() {
                    Ok(window) => window.title,
                    Err(_) => "Unknown System Window".to_string(), // Robust error handling
                };

                // Construct LogEvent
                let event = LogEvent {
                    timestamp: ts,
                    window_title,
                    event_type: "keypress".to_string(),
                };

                buffer.push(event);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Just a timeout, continue to check flush conditions
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed, shouldn't happen unless thread A crashes
                eprintln!("Sensor thread disconnected.");
                break;
            }
        }

        // Check flush conditions: Count OR Time
        let should_flush = buffer.len() >= BATCH_SIZE || last_flush.elapsed() >= FLUSH_INTERVAL;

        if should_flush && !buffer.is_empty() {
            flush_buffer(&buffer, &session_path)?;
            buffer.clear();
            last_flush = Instant::now();
        }
    }

    // Final flush before exit
    if !buffer.is_empty() {
        flush_buffer(&buffer, &session_path)?;
    }

    Ok(())
}

fn flush_buffer(buffer: &[LogEvent], path: &PathBuf) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("Failed to open session file")?;

    // We write each object as a separate JSON object line, or a list? 
    // The prompt says "Writes the buffer to ghostwriter_session.json". 
    // Appending a list of objects to a file usually breaks JSON validity if not handled as NDJSON or similar.
    // However, usually "appending to a log" implies NDJSON (Newline Delimited JSON).
    // Let's write them as individual lines for robustness (NDJSON).
    
    for event in buffer {
        let json = serde_json::to_string(event).context("Failed to serialize event")?;
        writeln!(file, "{}", json).context("Failed to write to file")?;
    }
    
    // println!("Flushed {} events.", buffer.len()); // Debug logging
    Ok(())
}
