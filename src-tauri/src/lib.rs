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
                if (!window.__mediaDurations__) window.__mediaDurations__ = {};
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
                                    const resp = JSON.parse(xhr.responseText);
                                    const returnedFileName = resp.file;
                                    const mediaUrl = 'http://127.0.0.1:12435/media/' + encodeURIComponent(returnedFileName);
                                    if (resp.duration != null) {
                                        window.__mediaDurations__[mediaUrl] = resp.duration;
                                        console.log("Pre-cached duration for", mediaUrl, ":", resp.duration, "s");
                                    }
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

/// Parse FLAC duration from raw file bytes.
/// Reads the STREAMINFO metadata block (always first, always 26 bytes into the file).
/// FLAC spec: https://xiph.org/flac/format.html#metadata_block_streaminfo
/// Zero dependencies, zero I/O — just pointer arithmetic on already-read bytes.
fn parse_flac_duration_from_bytes(bytes: &[u8]) -> Option<f64> {
    if bytes.len() < 26 { return None; }
    // Marker "fLaC"
    if &bytes[0..4] != b"fLaC" { return None; }
    // Byte 4: last-metadata flag (bit 7) + block type (bits 6-0). Must be 0 = STREAMINFO.
    if (bytes[4] & 0x7f) != 0 { return None; }
    // Sample rate: bits 0-19 of the 64-bit field starting at byte 18
    //   byte18 = sr[19..12], byte19 = sr[11..4], byte20[7..4] = sr[3..0]
    let sample_rate = ((bytes[18] as u32) << 12)
        | ((bytes[19] as u32) << 4)
        | ((bytes[20] as u32) >> 4);
    if sample_rate == 0 { return None; }
    // Total samples: bits 28-63 of the same 64-bit field
    //   byte21[3..0] = ts[35..32], bytes 22-25 = ts[31..0]
    let total_samples = (((bytes[21] & 0x0f) as u64) << 32)
        | ((bytes[22] as u64) << 24)
        | ((bytes[23] as u64) << 16)
        | ((bytes[24] as u64) << 8)
        |  (bytes[25] as u64);
    if total_samples == 0 { return None; }
    Some(total_samples as f64 / sample_rate as f64)
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

                    // Parse FLAC duration from already-read bytes (no extra I/O, no transcoding)
                    let flac_duration: Option<f64> = if ext == "flac" {
                        parse_flac_duration_from_bytes(&bytes)
                    } else {
                        None
                    };
                    
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
                    
                    // Return JSON {file, duration} so JS can pre-cache the duration
                    let duration_json = flac_duration
                        .map(|d| format!("{:.6}", d))
                        .unwrap_or_else(|| "null".to_string());
                    let safe_name = final_file_name.replace('\\', "\\\\").replace('"', "\\\"");
                    let json_body = format!("{{\"file\":\"{}\",\"duration\":{}}}", safe_name, duration_json);
                    let response = tiny_http::Response::from_string(json_body)
                        .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
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
                                        if parts[0].is_empty() && !parts[1].is_empty() {
                                            if let Ok(suffix_len) = parts[1].parse::<usize>() {
                                                range_start = file_len.saturating_sub(suffix_len);
                                                range_end = file_len - 1;
                                                has_range = true;
                                            }
                                        } else {
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
                                .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Expose-Headers"[..], &b"Content-Length, Content-Range, Accept-Ranges"[..]).unwrap())
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

#[tauri::command]
fn save_lyrics_dialog(window: tauri::WebviewWindow, lyrics_text: String, default_name: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        if let Ok(gtk_window) = window.gtk_window() {
            let file_chooser = gtk::FileChooserDialog::new(
                Some("儲存歌詞檔案"),
                Some(&gtk_window),
                gtk::FileChooserAction::Save,
            );
            file_chooser.add_button("取消", gtk::ResponseType::Cancel);
            file_chooser.add_button("儲存", gtk::ResponseType::Accept);
            file_chooser.set_do_overwrite_confirmation(true);
            file_chooser.set_current_name(&default_name);
            
            let filter = gtk::FileFilter::new();
            filter.add_pattern("*.lrc");
            filter.set_name(Some("LRC 歌詞檔案 (*.lrc)"));
            file_chooser.add_filter(filter);
            
            let response = file_chooser.run();
            if response == gtk::ResponseType::Accept {
                if let Some(filename) = file_chooser.filename() {
                    if let Ok(mut file) = std::fs::File::create(&filename) {
                        use std::io::Write;
                        let _ = file.write_all(lyrics_text.as_bytes());
                        file_chooser.close();
                        return Ok(());
                    } else {
                        file_chooser.close();
                        return Err("無法建立並寫入檔案".to_string());
                    }
                }
            }
            file_chooser.close();
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        let _ = lyrics_text;
        let _ = default_name;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct TitlebarWidgets {
    media_box: gtk::Box,
    sep1: gtk::Separator,
    lyrics_box: gtk::Box,
    sep2: gtk::Separator,
    history_box: gtk::Box,
    export_box: gtk::Box,
    sep3: gtk::Separator,
    subtitle_label: gtk::Label,

    clear_media_item: gtk::MenuItem,
    clear_lyrics_item: gtk::MenuItem,
    load_embedded_item: gtk::MenuItem,
    export_btn: gtk::Button,
    export_dropdown: gtk::MenuButton,

    // Undo/Redo buttons and their dropdown menus
    undo_btn: gtk::Button,
    redo_btn: gtk::Button,
    undo_dropdown: gtk::MenuButton,
    redo_dropdown: gtk::MenuButton,
    undo_menu: gtk::Menu,
    redo_menu: gtk::Menu,
}

#[cfg(target_os = "linux")]
thread_local! {
    static TITLEBAR_WIDGETS: std::cell::RefCell<Option<TitlebarWidgets>> = std::cell::RefCell::new(None);
}

#[tauri::command]
fn show_titlebar_buttons() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let _ = gtk::glib::idle_add_local(move || {
            TITLEBAR_WIDGETS.with(|widgets| {
                if let Some(w) = widgets.borrow().as_ref() {
                    w.media_box.show_all();
                    w.sep1.show();
                    w.lyrics_box.show_all();
                    w.sep2.show();
                    w.history_box.show_all();
                    w.export_box.show_all();
                    w.sep3.show();
                    w.subtitle_label.show();
                }
            });
            gtk::glib::ControlFlow::Break
        });
    }
    Ok(())
}

#[tauri::command]
fn set_titlebar_buttons_enabled(enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let _ = gtk::glib::idle_add_local(move || {
            TITLEBAR_WIDGETS.with(|widgets| {
                if let Some(w) = widgets.borrow().as_ref() {
                    // Set sensitivity on each interactive group as a whole.
                    // GTK propagates set_sensitive to all children automatically.
                    w.media_box.set_sensitive(enabled);
                    w.lyrics_box.set_sensitive(enabled);
                    w.history_box.set_sensitive(enabled);
                    w.export_box.set_sensitive(enabled);
                }
            });
            gtk::glib::ControlFlow::Break
        });
    }
    Ok(())
}

#[tauri::command]
fn on_app_state_changed(
    audio_file_name: Option<String>,
    lyric_file_name: Option<String>,
    can_clear_media: bool,
    can_clear_lyrics: bool,
    can_load_embedded_lyrics: bool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let _ = gtk::glib::idle_add_local(move || {
            TITLEBAR_WIDGETS.with(|widgets| {
                if let Some(w) = widgets.borrow().as_ref() {
                    let audio_str = audio_file_name.clone().unwrap_or_else(|| "(無)".to_string());
                    let lyric_str = lyric_file_name.clone().unwrap_or_else(|| {
                        if can_load_embedded_lyrics {
                            "內嵌標籤".to_string()
                        } else {
                            "(無)".to_string()
                        }
                    });
                    
                    let subtitle = format!("音訊: {} | 歌詞: {}", audio_str, lyric_str);
                    w.subtitle_label.set_text(&subtitle);
                    
                    w.clear_media_item.set_sensitive(can_clear_media);
                    w.clear_lyrics_item.set_sensitive(can_clear_lyrics);
                    w.load_embedded_item.set_sensitive(can_load_embedded_lyrics);
                    
                    w.export_btn.set_sensitive(can_clear_lyrics);
                    w.export_dropdown.set_sensitive(can_clear_lyrics);
                }
            });
            gtk::glib::ControlFlow::Break
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = audio_file_name;
        let _ = lyric_file_name;
        let _ = can_clear_media;
        let _ = can_clear_lyrics;
        let _ = can_load_embedded_lyrics;
    }
    Ok(())
}

#[tauri::command]
fn on_history_changed(
    window: tauri::WebviewWindow,
    can_undo: bool,
    can_redo: bool,
    undo_list: Vec<String>,
    redo_list: Vec<String>,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let _ = gtk::glib::idle_add_local(move || {
            TITLEBAR_WIDGETS.with(|widgets| {
                if let Some(w) = widgets.borrow().as_ref() {
                    // Update undo button & dropdown sensitivity
                    w.undo_btn.set_sensitive(can_undo);
                    w.undo_dropdown.set_sensitive(can_undo);

                    // Rebuild undo menu items
                    for child in w.undo_menu.children() {
                        w.undo_menu.remove(&child);
                    }
                    let webview = window.clone();
                    for (i, label) in undo_list.iter().enumerate() {
                        let steps = i + 1;
                        let item = gtk::MenuItem::with_label(&format!("復原到: {}", label));
                        let wv = webview.clone();
                        item.connect_activate(move |_| {
                            let _ = wv.eval(&format!(
                                "window.AppCommands && window.AppCommands.undoToSequence && window.AppCommands.undoToSequence({})",
                                steps
                            ));
                        });
                        w.undo_menu.append(&item);
                    }
                    w.undo_menu.show_all();

                    // Update redo button & dropdown sensitivity
                    w.redo_btn.set_sensitive(can_redo);
                    w.redo_dropdown.set_sensitive(can_redo);

                    // Rebuild redo menu items
                    for child in w.redo_menu.children() {
                        w.redo_menu.remove(&child);
                    }
                    let webview = window.clone();
                    for (i, label) in redo_list.iter().enumerate() {
                        let steps = i + 1;
                        let item = gtk::MenuItem::with_label(&format!("重複到: {}", label));
                        let wv = webview.clone();
                        item.connect_activate(move |_| {
                            let _ = wv.eval(&format!(
                                "window.AppCommands && window.AppCommands.redoToSequence && window.AppCommands.redoToSequence({})",
                                steps
                            ));
                        });
                        w.redo_menu.append(&item);
                    }
                    w.redo_menu.show_all();
                }
            });
            gtk::glib::ControlFlow::Break
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        let _ = can_undo;
        let _ = can_redo;
        let _ = undo_list;
        let _ = redo_list;
    }
    Ok(())
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

            // Create a custom two-line title widget inside a GtkEventBox
            let title_box = gtk::EventBox::new();
            title_box.set_visible_window(false); // Transparent background
            title_box.set_hexpand(true); // Fill all empty horizontal space
            title_box.set_vexpand(true); // Fill 100% height of the HeaderBar
            title_box.set_valign(gtk::Align::Fill);
            title_box.set_margin_top(0);
            title_box.set_margin_bottom(0);
            
            // Vertical box to stack Title and Subtitle
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
            vbox.set_valign(gtk::Align::Center); // Centered vertically
            vbox.set_halign(gtk::Align::Center); // Centered horizontally
            
            let title_label = gtk::Label::new(Some("LRC Maker Enhanced"));
            title_label.style_context().add_class("title");
            
            let subtitle_label = gtk::Label::new(Some("音訊: (無) | 歌詞: (無)"));
            subtitle_label.set_visible(false);
            
            // Apply custom premium CSS styles to remove margins and style text beautifully
            let css_provider = gtk::CssProvider::new();
            let _ = css_provider.load_from_data(
                b"
                headerbar {
                    min-height: 0px;
                    padding-top: 0px;
                    padding-bottom: 0px;
                    margin-top: 0px;
                    margin-bottom: 0px;
                }
                .custom-title-label {
                    font-weight: bold;
                    font-size: 13px;
                }
                .custom-subtitle-label {
                    font-size: 10px;
                    opacity: 0.65;
                }
                "
            );
            header_bar.style_context().add_provider(&css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            
            title_label.style_context().add_class("custom-title-label");
            subtitle_label.style_context().add_class("custom-subtitle-label");
            title_label.style_context().add_provider(&css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            subtitle_label.style_context().add_provider(&css_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            
            vbox.pack_start(&title_label, false, false, 0);
            vbox.pack_start(&subtitle_label, false, false, 0);
            
            title_box.add(&vbox);
            header_bar.set_custom_title(Some(&title_box));

            // 1. Media Actions (Linked Box)
            let media_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            media_box.style_context().add_class("linked");
            
            let load_media_btn = gtk::Button::with_label("載入媒體");
            let webview_clone = webview_window.clone();
            load_media_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadMedia && window.AppCommands.loadMedia()");
            });
            media_box.pack_start(&load_media_btn, false, false, 0);

            // Create media dropdown menu
            let media_menu = gtk::Menu::new();
            let clear_media_item = gtk::MenuItem::with_label("清除媒體");
            let webview_clone = webview_window.clone();
            clear_media_item.connect_activate(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.clearMedia && window.AppCommands.clearMedia()");
            });
            media_menu.append(&clear_media_item);
            media_menu.show_all();

            let media_dropdown = gtk::MenuButton::new();
            media_dropdown.set_popup(Some(&media_menu));
            media_dropdown.set_tooltip_text(Some("媒體選項"));
            media_box.pack_start(&media_dropdown, false, false, 0);

            header_bar.pack_start(&media_box);

            // Separator 1
            let sep1 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_start(&sep1);

            // 2. Lyrics Actions (Linked Box)
            let lyrics_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            lyrics_box.style_context().add_class("linked");

            let load_lyrics_btn = gtk::Button::with_label("載入歌詞");
            let webview_clone = webview_window.clone();
            load_lyrics_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadLyrics && window.AppCommands.loadLyrics()");
            });
            lyrics_box.pack_start(&load_lyrics_btn, false, false, 0);

            // Create lyrics dropdown menu (aligned with TopToolbar.tsx order)
            let lyrics_menu = gtk::Menu::new();

            // 1. 載入歌詞檔案
            let load_lyrics_menu_item = gtk::MenuItem::with_label("載入歌詞檔案");
            let webview_clone = webview_window.clone();
            load_lyrics_menu_item.connect_activate(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadLyrics && window.AppCommands.loadLyrics()");
            });
            lyrics_menu.append(&load_lyrics_menu_item);

            // 2. 載入內嵌標籤
            let load_embedded_item = gtk::MenuItem::with_label("載入內嵌標籤");
            let webview_clone = webview_window.clone();
            load_embedded_item.connect_activate(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadEmbeddedLyrics && window.AppCommands.loadEmbeddedLyrics()");
            });
            lyrics_menu.append(&load_embedded_item);

            // 3. 分隔線
            let lyrics_sep = gtk::SeparatorMenuItem::new();
            lyrics_menu.append(&lyrics_sep);

            // 4. 清除歌詞
            let clear_lyrics_item = gtk::MenuItem::with_label("清除歌詞");
            let webview_clone = webview_window.clone();
            clear_lyrics_item.connect_activate(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.clearLyrics && window.AppCommands.clearLyrics()");
            });
            lyrics_menu.append(&clear_lyrics_item);
            lyrics_menu.show_all();

            let lyrics_dropdown = gtk::MenuButton::new();
            lyrics_dropdown.set_popup(Some(&lyrics_menu));
            lyrics_dropdown.set_tooltip_text(Some("歌詞選項"));
            lyrics_box.pack_start(&lyrics_dropdown, false, false, 0);

            header_bar.pack_start(&lyrics_box);

            // Separator 2
            let sep2 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_start(&sep2);

            // 3. Undo/Redo Box (Linked Box)
            let history_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            history_box.style_context().add_class("linked");

            // Undo button
            let undo_btn = gtk::Button::from_icon_name(Some("edit-undo-symbolic"), gtk::IconSize::Button);
            undo_btn.set_tooltip_text(Some("復原"));
            undo_btn.set_sensitive(false);
            let webview_clone = webview_window.clone();
            undo_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.undo && window.AppCommands.undo()");
            });
            history_box.pack_start(&undo_btn, false, false, 0);

            // Undo dropdown menu
            let undo_menu = gtk::Menu::new();
            let undo_dropdown = gtk::MenuButton::new();
            undo_dropdown.set_popup(Some(&undo_menu));
            undo_dropdown.set_sensitive(false);
            undo_dropdown.set_tooltip_text(Some("復原歷史記錄"));
            history_box.pack_start(&undo_dropdown, false, false, 0);

            // Redo button
            let redo_btn = gtk::Button::from_icon_name(Some("edit-redo-symbolic"), gtk::IconSize::Button);
            redo_btn.set_tooltip_text(Some("重複"));
            redo_btn.set_sensitive(false);
            let webview_clone = webview_window.clone();
            redo_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.redo && window.AppCommands.redo()");
            });
            history_box.pack_start(&redo_btn, false, false, 0);

            // Redo dropdown menu
            let redo_menu = gtk::Menu::new();
            let redo_dropdown = gtk::MenuButton::new();
            redo_dropdown.set_popup(Some(&redo_menu));
            redo_dropdown.set_sensitive(false);
            redo_dropdown.set_tooltip_text(Some("重複歷史記錄"));
            history_box.pack_start(&redo_dropdown, false, false, 0);

            header_bar.pack_start(&history_box);

            // 4. Export Actions (Linked Box on Right side)
            let export_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            export_box.style_context().add_class("linked");

            let export_btn = gtk::Button::with_label("匯出 .lrc");
            let webview_clone = webview_window.clone();
            export_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.exportCurrent && window.AppCommands.exportCurrent()");
            });
            export_box.pack_start(&export_btn, false, false, 0);

            // Create export dropdown menu (aligned with TopToolbar.tsx)
            let export_menu = gtk::Menu::new();

            // 標準匯出 (.lrc)
            let export_standard_item = gtk::MenuItem::with_label("標準 LRC (行同步)");
            let webview_clone = webview_window.clone();
            export_standard_item.connect_activate(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.exportStandard && window.AppCommands.exportStandard()");
            });
            export_menu.append(&export_standard_item);

            // 加強匯出 (.elrc)
            let export_enhanced_item = gtk::MenuItem::with_label("逐字版 LRC (ESLyric - 逐字同步)");
            let webview_clone = webview_window.clone();
            export_enhanced_item.connect_activate(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.exportEnhanced && window.AppCommands.exportEnhanced()");
            });
            export_menu.append(&export_enhanced_item);
            export_menu.show_all();

            let export_dropdown = gtk::MenuButton::new();
            export_dropdown.set_popup(Some(&export_menu));
            export_dropdown.set_tooltip_text(Some("匯出選項"));
            export_box.pack_start(&export_dropdown, false, false, 0);

            header_bar.pack_end(&export_box);

            // Separator 3 (Right side)
            let sep3 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_end(&sep3);

            // Enable native window dragging & double-click to maximize on the entire title/subtitle area
            let gtk_window_clone = gtk_window.clone();
            let webview_clone = webview_window.clone();
            title_box.connect_button_press_event(move |_, event| {
                if event.button() == 1 { // Left click
                    if event.event_type() == gtk::gdk::EventType::DoubleButtonPress {
                        if gtk_window_clone.is_maximized() {
                            gtk_window_clone.unmaximize();
                        } else {
                            gtk_window_clone.maximize();
                        }
                        return gtk::glib::Propagation::Stop;
                    }
                    
                    let _ = webview_clone.start_dragging();
                }
                gtk::glib::Propagation::Proceed
            });

            // Set initial visibility of all buttons to false
            media_box.set_visible(false);
            sep1.set_visible(false);
            lyrics_box.set_visible(false);
            sep2.set_visible(false);
            history_box.set_visible(false);
            export_box.set_visible(false);
            sep3.set_visible(false);

            // Store widgets in thread-local for show_titlebar_buttons command
            TITLEBAR_WIDGETS.with(|widgets| {
                *widgets.borrow_mut() = Some(TitlebarWidgets {
                    media_box,
                    sep1,
                    lyrics_box,
                    sep2,
                    history_box,
                    export_box,
                    sep3,
                    subtitle_label,
                    clear_media_item,
                    clear_lyrics_item,
                    load_embedded_item,
                    export_btn,
                    export_dropdown,
                    undo_btn,
                    redo_btn,
                    undo_dropdown,
                    redo_dropdown,
                    undo_menu,
                    redo_menu,
                });
            });

            // Set the new HeaderBar as the titlebar of the GTK window
            gtk_window.set_titlebar(Some(&header_bar));
            
            // Show only the header_bar container and the custom title_box, keeping all button widgets hidden
            header_bar.show();
            title_box.show_all();
            println!("Successfully configured custom Linux GTK3 HeaderBar (initially hidden buttons) with drag support!");
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
        .invoke_handler(tauri::generate_handler![greet, read_file_binary, save_lyrics_dialog, show_titlebar_buttons, set_titlebar_buttons_enabled, on_app_state_changed, on_history_changed])
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

