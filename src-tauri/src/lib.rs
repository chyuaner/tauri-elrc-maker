use std::thread;
use std::io::{Read, Write, Seek};

struct GStreamerFixPlugin;

impl<R: tauri::Runtime> tauri::plugin::Plugin<R> for GStreamerFixPlugin {
    fn name(&self) -> &'static str {
        "gstreamer-fix"
    }

    fn initialization_script(&self) -> Option<String> {
        Some(r#"
            (function() {
                if (window.__ObjectURL_Patched__) return;
                window.__ObjectURL_Patched__ = true;
                const originalCreateObjectURL = URL.createObjectURL;
                URL.createObjectURL = function(obj) {
                    if (obj instanceof Blob || obj instanceof File) {
                        if (obj.type.startsWith('audio/') || obj.type.startsWith('video/')) {
                            console.log("Intercepted media file creation:", obj.name, obj.type);
                            try {
                                const xhr = new XMLHttpRequest();
                                xhr.open('POST', 'http://127.0.0.1:12435/save', false);
                                xhr.setRequestHeader('X-File-Name', encodeURIComponent(obj.name || 'temp_media'));
                                xhr.send(obj);
                                if (xhr.status === 200) {
                                    const returnedFileName = xhr.responseText;
                                    const mediaUrl = 'http://127.0.0.1:12435/media/' + encodeURIComponent(returnedFileName);
                                    console.log("Successfully mapped blob to standard local HTTP media URL:", mediaUrl);
                                    return mediaUrl;
                                }
                            } catch (e) {
                                console.error("Failed to save media via sync XHR:", e);
                            }
                        }
                    }
                    return originalCreateObjectURL.apply(this, arguments);
                };
                console.log("URL.createObjectURL successfully monkeypatched for GStreamer compatibility!");
            })();
        "#.to_string())
    }
}

pub fn start_http_server() {
    thread::spawn(|| {
        if let Ok(server) = tiny_http::Server::http("127.0.0.1:12435") {
            println!("Local GStreamer temp-media sync server started on port 12435!");
            for mut request in server.incoming_requests() {
                let url = request.url().to_string();
                let method = request.method().clone();
                
                if url == "/save" && method == tiny_http::Method::Post {
                    // --- SAVE ENDPOINT ---
                    let file_name = request.headers().iter()
                        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("X-File-Name"))
                        .map(|h| h.value.as_str())
                        .and_then(|s| percent_encoding::percent_decode_str(s).decode_utf8().ok())
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|| "temp_media.mp3".to_string());
                    
                    let temp_dir = std::env::temp_dir();
                    let file_path = temp_dir.join(&file_name);
                    
                    let mut bytes = Vec::new();
                    let _ = request.as_reader().read_to_end(&mut bytes);
                    
                    if let Ok(mut file) = std::fs::File::create(&file_path) {
                        let _ = file.write_all(&bytes);
                    }
                    
                    let file_path_str = file_path.to_string_lossy().into_owned();
                    println!("Saved temp media file: {}", file_path_str);
                    
                    // Check if file is m4a or aac, and instantly transcode to WAV via native ffmpeg
                    let ext = file_path.extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    
                    let mut final_file_name = file_name.clone();
                    
                    if ext == "m4a" || ext == "aac" {
                        let wav_file_name = file_name
                            .replace(".m4a", ".wav")
                            .replace(".M4A", ".wav")
                            .replace(".aac", ".wav")
                            .replace(".AAC", ".wav");
                        let wav_file_path = temp_dir.join(&wav_file_name);
                        
                        println!("Auto-transcoding M4A/AAC to WAV: {} -> {}", file_path.display(), wav_file_path.display());
                        
                        let status = std::process::Command::new("ffmpeg")
                            .arg("-y")
                            .arg("-i")
                            .arg(&file_path)
                            .arg("-c:a")
                            .arg("pcm_s16le")
                            .arg(&wav_file_path)
                            .status();
                        
                        if let Ok(stat) = status {
                            if stat.success() {
                                println!("Transcode successful! Returning WAV filename.");
                                final_file_name = wav_file_name;
                            } else {
                                println!("FFmpeg transcode returned error status.");
                            }
                        } else {
                            println!("Failed to execute FFmpeg command.");
                        }
                    }
                    
                    // Return the filename so the JS can build the URL
                    let response = tiny_http::Response::from_string(final_file_name)
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap());
                    let _ = request.respond(response);
                    
                } else if url.starts_with("/media/") && method == tiny_http::Method::Get {
                    // --- MEDIA SERVING ENDPOINT ---
                    let file_name_encoded = &url["/media/".len()..];
                    let file_name = percent_encoding::percent_decode_str(file_name_encoded)
                        .decode_utf8()
                        .ok()
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|| "".to_string());
                    
                    let temp_dir = std::env::temp_dir();
                    let file_path = temp_dir.join(&file_name);
                    
                    if file_path.exists() && file_path.is_file() {
                        if let Ok(mut file) = std::fs::File::open(&file_path) {
                            let metadata = file.metadata().unwrap();
                            let file_len = metadata.len() as usize;
                            
                            // Guess Content-Type based on extension
                            let ext = file_path.extension()
                                .and_then(|s| s.to_str())
                                .unwrap_or("mp3")
                                .to_lowercase();
                            let content_type = match ext.as_str() {
                                "wav" => "audio/wav",
                                "flac" => "audio/flac",
                                "mp3" => "audio/mpeg",
                                "m4a" => "video/quicktime",
                                "aac" => "audio/aac",
                                "ogg" | "oga" => "audio/ogg",
                                "mp4" => "video/mp4",
                                "mkv" => "video/x-matroska",
                                "webm" => "video/webm",
                                _ => "application/octet-stream",
                            };
                            
                            // Check for Range header
                            let mut range_start = 0;
                            let mut range_end = file_len - 1;
                            let mut has_range = false;
                            
                            if let Some(range_header) = request.headers().iter()
                                .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("range"))
                            {
                                let range_str = range_header.value.as_str();
                                if range_str.starts_with("bytes=") {
                                    let clean_range = &range_str["bytes=".len()..];
                                    let parts: Vec<&str> = clean_range.split('-').collect();
                                    if parts.len() >= 2 {
                                        if let Ok(start) = parts[0].parse::<usize>() {
                                            range_start = start;
                                            has_range = true;
                                        }
                                        if !parts[1].is_empty() {
                                            if let Ok(end) = parts[1].parse::<usize>() {
                                                range_end = end;
                                                has_range = true;
                                            }
                                        }
                                    }
                                }
                            }
                            
                            // Secure range bounds
                            if range_start >= file_len {
                                range_start = file_len - 1;
                            }
                            if range_end >= file_len {
                                range_end = file_len - 1;
                            }
                            if range_start > range_end {
                                range_start = range_end;
                            }
                            
                            let chunk_size = range_end - range_start + 1;
                            
                            // Read chunk
                            let _ = file.seek(std::io::SeekFrom::Start(range_start as u64));
                            let mut buffer = vec![0u8; chunk_size];
                            let _ = file.read_exact(&mut buffer);
                            
                            let mut response = tiny_http::Response::from_data(buffer)
                                .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap())
                                .with_header(tiny_http::Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap())
                                .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap());
                            
                            if has_range {
                                let content_range = format!("bytes {}-{}/{}", range_start, range_end, file_len);
                                response = response
                                    .with_status_code(206)
                                    .with_header(tiny_http::Header::from_bytes(&b"Content-Range"[..], content_range.as_bytes()).unwrap());
                                println!("Serving HTTP 206 Range: {}-{} / {}", range_start, range_end, file_len);
                            } else {
                                response = response.with_status_code(200);
                                println!("Serving HTTP 200 Full: size={}", file_len);
                            }
                            
                            let _ = request.respond(response);
                        } else {
                            let response = tiny_http::Response::from_string("Error opening file")
                                .with_status_code(500)
                                .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap());
                            let _ = request.respond(response);
                        }
                    } else {
                        let response = tiny_http::Response::from_string("File not found")
                            .with_status_code(404)
                            .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap());
                        let _ = request.respond(response);
                    }
                    
                } else if method == tiny_http::Method::Options {
                    // --- CORS PREFLIGHT ---
                    let response = tiny_http::Response::from_string("OK")
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap())
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"*"[..]).unwrap())
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"POST, GET, OPTIONS"[..]).unwrap());
                    let _ = request.respond(response);
                    
                } else {
                    let response = tiny_http::Response::from_string("Not Found")
                        .with_status_code(404)
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap());
                    let _ = request.respond(response);
                }
            }
        }
    });
}

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[tauri::command]
fn read_file_binary(path: String) -> Result<tauri::ipc::Response, String> {
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(tauri::ipc::Response::new(bytes))
}

#[cfg(target_os = "linux")]
fn setup_linux_titlebar(app: &mut tauri::App) {
    use tauri::Manager;
    use gtk::prelude::*;

    if let Some(window) = app.get_webview_window("main") {
        if let Ok(gtk_window) = window.gtk_window() {
            let webview_window = window.clone();
            
            // Create custom HeaderBar
            let header_bar = gtk::HeaderBar::new();
            header_bar.set_show_close_button(true);
            header_bar.set_title(Some("LRC Maker Enhanced"));

            // 1. Media Actions Box
            let media_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            
            let load_media_btn = gtk::Button::with_label("載入媒體");
            let webview_clone = webview_window.clone();
            load_media_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadMedia && window.AppCommands.loadMedia()");
            });
            media_box.pack_start(&load_media_btn, false, false, 0);

            let clear_media_btn = gtk::Button::with_label("清除媒體");
            let webview_clone = webview_window.clone();
            clear_media_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.clearMedia && window.AppCommands.clearMedia()");
            });
            media_box.pack_start(&clear_media_btn, false, false, 0);

            header_bar.pack_start(&media_box);

            // Separator 1
            let sep1 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_start(&sep1);

            // 2. Lyrics Actions Box
            let lyrics_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);

            let load_lyrics_btn = gtk::Button::with_label("載入歌詞");
            let webview_clone = webview_window.clone();
            load_lyrics_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadLyrics && window.AppCommands.loadLyrics()");
            });
            lyrics_box.pack_start(&load_lyrics_btn, false, false, 0);

            let load_embedded_btn = gtk::Button::with_label("載入內嵌");
            let webview_clone = webview_window.clone();
            load_embedded_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadEmbeddedLyrics && window.AppCommands.loadEmbeddedLyrics()");
            });
            lyrics_box.pack_start(&load_embedded_btn, false, false, 0);

            let clear_lyrics_btn = gtk::Button::with_label("清除歌詞");
            let webview_clone = webview_window.clone();
            clear_lyrics_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.clearLyrics && window.AppCommands.clearLyrics()");
            });
            lyrics_box.pack_start(&clear_lyrics_btn, false, false, 0);

            header_bar.pack_start(&lyrics_box);

            // Separator 2
            let sep2 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_start(&sep2);

            // 3. Undo/Redo Box
            let history_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);

            let undo_btn = gtk::Button::with_label("復原");
            let webview_clone = webview_window.clone();
            undo_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.undo && window.AppCommands.undo()");
            });
            history_box.pack_start(&undo_btn, false, false, 0);

            let redo_btn = gtk::Button::with_label("重做");
            let webview_clone = webview_window.clone();
            redo_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.redo && window.AppCommands.redo()");
            });
            history_box.pack_start(&redo_btn, false, false, 0);

            header_bar.pack_start(&history_box);

            // 4. Export Actions Box (Right side)
            let export_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);

            let export_standard_btn = gtk::Button::with_label("標準匯出");
            let webview_clone = webview_window.clone();
            export_standard_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.exportStandard && window.AppCommands.exportStandard()");
            });
            export_box.pack_start(&export_standard_btn, false, false, 0);

            let export_enhanced_btn = gtk::Button::with_label("加強匯出");
            let webview_clone = webview_window.clone();
            export_enhanced_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.exportEnhanced && window.AppCommands.exportEnhanced()");
            });
            export_box.pack_start(&export_enhanced_btn, false, false, 0);

            header_bar.pack_end(&export_box);

            // Separator 3 (Right side)
            let sep3 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_end(&sep3);

            // 5. Offset Box (Right side)
            let offset_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);

            let offset_btn = gtk::Button::with_label("時間偏移");
            let webview_clone = webview_window.clone();
            offset_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.shiftTime && window.AppCommands.shiftTime()");
            });
            offset_box.pack_start(&offset_btn, false, false, 0);

            header_bar.pack_end(&offset_box);

            // Set the new HeaderBar as the titlebar of the GTK window
            gtk_window.set_titlebar(Some(&header_bar));
            header_bar.show_all();
            println!("Successfully configured custom Linux GTK3 HeaderBar!");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Start local media sync HTTP server
    start_http_server();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(GStreamerFixPlugin)
        .invoke_handler(tauri::generate_handler![greet, read_file_binary])
        .setup(|app| {
            #[cfg(target_os = "linux")]
            {
                setup_linux_titlebar(app);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

