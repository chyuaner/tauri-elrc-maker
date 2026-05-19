use std::thread;
use std::io::{Read, Write, Seek};

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║  [LINUX WORKAROUND] GStreamer + WebKitGTK 媒體播放相容性修正              ║
// ╠══════════════════════════════════════════════════════════════════════════╣
// ║  問題：Tauri on Linux 使用 WebKitGTK，底層媒體解碼依賴 GStreamer。         ║
// ║        GStreamer 無法播放 Blob URL（blob:// scheme），                    ║
// ║        導致前端 URL.createObjectURL(file) 建立的音訊 URL 無法播放。        ║
// ║                                                                          ║
// ║  解法（Workaround）：                                                     ║
// ║    1. 本 Plugin 在頁面初始化時注入 JS，monkey-patch URL.createObjectURL   ║
// ║    2. 攔截到媒體 Blob 時，同步 POST 到本機 HTTP Server（port 12435）      ║
// ║    3. Server 儲存為 /tmp/ 暫存檔，回傳 http://127.0.0.1:12435/media/URL  ║
// ║    4. 前端改用此標準 HTTP URL 播放，GStreamer 可正常串流                   ║
// ╚══════════════════════════════════════════════════════════════════════════╝
struct GStreamerFixPlugin;

impl<R: tauri::Runtime> tauri::plugin::Plugin<R> for GStreamerFixPlugin {
    fn name(&self) -> &'static str {
        "gstreamer-fix"
    }

    /// [LINUX WORKAROUND] 在 WebView 頁面載入前注入的 JS 初始化腳本。
    /// Monkey-patch URL.createObjectURL，將媒體 Blob 轉發到本機 HTTP Server，
    /// 回傳 GStreamer 可播放的 http://127.0.0.1:12435/media/ URL。
    fn initialization_script(&self) -> Option<String> {
        Some(r#"
            (function() {
                // 防止 SPA 路由切換時重複 patch
                if (window.__ObjectURL_Patched__) return;
                window.__ObjectURL_Patched__ = true;

                // 快取各 HTTP URL 對應的音訊時長（秒），
                // 修正 GStreamer 串流模式下 <audio>.duration 回傳 Infinity 的問題
                if (!window.__mediaDurations__) window.__mediaDurations__ = {};

                const originalCreateObjectURL = URL.createObjectURL;

                // [WORKAROUND] 攔截 createObjectURL，替換成本機 HTTP URL
                URL.createObjectURL = function(obj) {
                    if (obj instanceof Blob || obj instanceof File) {
                        if (obj.type.startsWith('audio/') || obj.type.startsWith('video/')) {
                            console.log("Intercepted media file creation:", obj.name, obj.type);
                            try {
                                // 必須用同步 XHR，因為 createObjectURL 本身是同步 API
                                // 將 Blob POST 到 Rust HTTP Server 儲存成 /tmp/ 暫存檔
                                const xhr = new XMLHttpRequest();
                                xhr.open('POST', 'http://127.0.0.1:12435/save', false);
                                xhr.setRequestHeader('X-File-Name', encodeURIComponent(obj.name || 'temp_media'));
                                xhr.send(obj);
                                if (xhr.status === 200) {
                                    const resp = JSON.parse(xhr.responseText);
                                    const returnedFileName = resp.file;
                                    // 改用標準 HTTP URL，GStreamer 可正常串流
                                    const mediaUrl = 'http://127.0.0.1:12435/media/' + encodeURIComponent(returnedFileName);
                                    // 若 Server 回傳了預先解析的時長（目前僅 FLAC），快取起來
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
                    return originalCreateObjectURL.apply(this, arguments); // 非媒體或失敗時走原始流程
                };
                console.log("URL.createObjectURL successfully monkeypatched for GStreamer compatibility!");
            })();
        "#.to_string())
    }
}

// ╔══════════════════════════════════════════════════════════════════════════╗
// ║  [LINUX WORKAROUND] FLAC 時長解析（無 transcoding，純位元組計算）          ║
// ╠══════════════════════════════════════════════════════════════════════════╣
// ║  問題：GStreamer 以串流模式播放時，<audio>.duration 回傳 Infinity。         ║
// ║        FLAC header（STREAMINFO block）包含取樣率和總樣本數，可直接計算。    ║
// ║        在 /save 時順便解析並回傳給前端快取，完全繞過這個限制。               ║
// ║  注意：MP3/WAV/OGG 目前無此處理，duration 可能仍顯示 Infinity。            ║
// ╚══════════════════════════════════════════════════════════════════════════╝

/// 從 FLAC 原始位元組解析時長（秒）。讀取 STREAMINFO block（byte 4-25）。
/// 參考：<https://xiph.org/flac/format.html#metadata_block_streaminfo>
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
    // ╔══════════════════════════════════════════════════════════════════════╗
    // ║  [LINUX WORKAROUND] 本機 HTTP 媒體串流伺服器（port 12435）           ║
    // ╠══════════════════════════════════════════════════════════════════════╣
    // ║  為何不用 Tauri 自訂協議（asset://）？                               ║
    // ║    asset:// 不支援 HTTP Range 請求，GStreamer seek 會失效。           ║
    // ║  API：POST /save（接收Blob）、GET /media/<file>（串流回傳）           ║
    // ║  ⚠ 暫存檔存於 /tmp/，不會自動清除，重開機才消失。                    ║
    // ╚══════════════════════════════════════════════════════════════════════╝
    thread::spawn(|| {
        if let Ok(server) = tiny_http::Server::http("127.0.0.1:12435") {
            println!("Local GStreamer temp-media sync server started on port 12435!");
            for mut request in server.incoming_requests() {
                let url = request.url().to_string();
                let method = request.method().clone();

                // ── POST /save：接收 Blob、儲存暫存檔、視需要轉檔 ────────────
                if url == "/save" && method == tiny_http::Method::Post {
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
                    
                    println!("Saved temp media file: {}", file_path.display());

                    // [WORKAROUND] M4A/AAC → WAV 自動轉檔
                    // 原因：GStreamer 播放 M4A/AAC 需要 gstreamer1.0-plugins-bad 或
                    //       gstreamer1.0-libav，多數 Linux 預設未安裝。
                    //       轉成 PCM WAV 可完全避免此依賴問題。
                    // ⚠ 需求：系統需安裝 ffmpeg（`sudo apt install ffmpeg`）。
                    let ext = file_path.extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase();

                    let mut final_file_name = file_name.clone();

                    // [WORKAROUND] FLAC 時長預解析
                    // GStreamer 串流 FLAC 時 duration=Infinity，在此預先計算並回傳前端快取
                    let flac_duration: Option<f64> = if ext == "flac" {
                        parse_flac_duration_from_bytes(&bytes)
                    } else {
                        None
                    };
                    
                    if ext == "m4a" || ext == "aac" {
                        let wav_file_name = file_name
                            .replace(".m4a", ".wav").replace(".M4A", ".wav")
                            .replace(".aac", ".wav").replace(".AAC", ".wav");
                        let wav_file_path = temp_dir.join(&wav_file_name);
                        println!("Auto-transcoding M4A/AAC to WAV: {} -> {}", file_path.display(), wav_file_path.display());
                        // -y 覆蓋輸出檔；-c:a pcm_s16le = 16-bit PCM（GStreamer 原生支援）
                        let status = std::process::Command::new("ffmpeg")
                            .arg("-y")
                            .arg("-i").arg(&file_path)
                            .arg("-c:a").arg("pcm_s16le")
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
                            println!("Failed to execute FFmpeg command (is ffmpeg installed?).");
                        }
                    }
                    
                    // 回傳 JSON {file, duration}：前端用 file 組 /media/ URL，
                    // duration 用來修正 GStreamer 串流模式下 <audio>.duration=Infinity 的問題
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
                    // ── GET /media/<file>：串流回傳暫存檔，支援 HTTP Range ────
                    // HTTP Range（RFC 7233）是拖曳進度列的必要條件；
                    // GStreamer seek 時會發 Range: bytes=N-，不支援則無法拖曳。
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

/// 儲存所有需要在執行期動態控制的 GTK 標題列 widget 參照。
/// 因為 GTK 必須在主執行緒操作，所以使用 thread_local! 包裝。
/// 前端呼叫 Tauri command 後，透過 idle_add_local 回到 GTK 主執行緒讀取此結構體。
#[cfg(target_os = "linux")]
struct TitlebarWidgets {
    // ── 可見性控制：startup 時全部隱藏，等前端 ready 才顯示 ──
    media_box:  gtk::Box,       // 「媒體」按鈕群組
    sep1:       gtk::Separator, // 媒體 | 歌詞 之間的垂直分隔線
    lyrics_box: gtk::Box,       // 「歌詞」按鈕群組
    sep2:       gtk::Separator, // 歌詞 | 歷史 之間的垂直分隔線
    history_box: gtk::Box,      // 「復原/重複」按鈕群組
    export_box: gtk::Box,       // 「匯出」按鈕群組（右側）
    sep3:       gtk::Separator, // 匯出左側的垂直分隔線（右側 pack_end）
    subtitle_label: gtk::Label, // 標題列中央的副標題（顯示音訊/歌詞檔名）

    // ── 需要動態啟用/停用的選單項目（由 on_app_state_changed 控制）──
    clear_media_item:   gtk::MenuItem, // 清除媒體（有媒體時才啟用）
    clear_lyrics_item:  gtk::MenuItem, // 清除歌詞（有歌詞時才啟用）
    load_embedded_item: gtk::MenuItem, // 載入內嵌標籤（有內嵌歌詞時才啟用）
    export_btn:      gtk::Button,      // 匯出主按鈕（有歌詞時才啟用）
    export_dropdown: gtk::MenuButton,  // 匯出下拉箭頭（有歌詞時才啟用）

    // ── 復原/重複：按鈕 + 下拉選單（由 on_history_changed 動態重建項目）──
    undo_btn:      gtk::Button,     // 復原主按鈕（圖示）
    redo_btn:      gtk::Button,     // 重複主按鈕（圖示）
    undo_dropdown: gtk::MenuButton, // 復原歷史清單下拉箭頭
    redo_dropdown: gtk::MenuButton, // 重複歷史清單下拉箭頭
    undo_menu: gtk::Menu,           // 復原歷史 GtkMenu（每次 on_history_changed 清空重建）
    redo_menu: gtk::Menu,           // 重複歷史 GtkMenu（每次 on_history_changed 清空重建）
}

// thread_local 讓 TitlebarWidgets 只存在於 GTK 主執行緒，避免跨執行緒存取 GTK widget。
#[cfg(target_os = "linux")]
thread_local! {
    static TITLEBAR_WIDGETS: std::cell::RefCell<Option<TitlebarWidgets>> = std::cell::RefCell::new(None);
}

/// [Tauri command] 前端 Ready 後呼叫此命令，讓標題列所有按鈕/分隔線一次顯示。
/// 在 setup 階段這些 widget 都是隱藏的，避免 WebView 尚未載入時顯示無效的按鈕。
#[tauri::command]
fn show_titlebar_buttons() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        // idle_add_local 確保 UI 操作在 GTK 主執行緒執行（Tauri command 在 async 執行緒）
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
            gtk::glib::ControlFlow::Break // 執行一次即停止，不重複排程
        });
    }
    Ok(())
}

/// [Tauri command] 當 Dialog（確認框）開啟時，停用標題列所有按鈕，防止操作衝突。
/// GTK 的 set_sensitive(false) 會自動遞迴停用容器內所有子 widget，
/// 所以只需對各 Box 容器操作即可，不必個別設定每顆按鈕。
#[tauri::command]
fn set_titlebar_buttons_enabled(enabled: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::*;
        let _ = gtk::glib::idle_add_local(move || {
            TITLEBAR_WIDGETS.with(|widgets| {
                if let Some(w) = widgets.borrow().as_ref() {
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

/// [Tauri command] 前端每次狀態改變（載入/清除媒體或歌詞）都會呼叫此命令，
/// 同步更新標題列副標題文字，並根據當前狀態啟用或停用對應的選單項目與按鈕。
///
/// 參數說明：
/// - audio_file_name / lyric_file_name：顯示在副標題的檔名（None 表示未載入）
/// - can_clear_media：是否有媒體可清除（控制「清除媒體」選單項目）
/// - can_clear_lyrics：是否有歌詞可操作（控制「清除歌詞」及匯出按鈕）
/// - can_load_embedded_lyrics：媒體的 tag 內是否有內嵌歌詞（控制「載入內嵌標籤」）
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
                    // 組合副標題文字；若無歌詞檔但有內嵌標籤則顯示「內嵌標籤」
                    let audio_str = audio_file_name.clone().unwrap_or_else(|| "(無)".to_string());
                    let lyric_str = lyric_file_name.clone().unwrap_or_else(|| {
                        if can_load_embedded_lyrics { "內嵌標籤".to_string() }
                        else { "(無)".to_string() }
                    });
                    w.subtitle_label.set_text(&format!("音訊: {} | 歌詞: {}", audio_str, lyric_str));

                    // 根據狀態啟用/停用各選單項目
                    w.clear_media_item.set_sensitive(can_clear_media);
                    w.clear_lyrics_item.set_sensitive(can_clear_lyrics);
                    w.load_embedded_item.set_sensitive(can_load_embedded_lyrics);

                    // 匯出按鈕與下拉箭頭：只有在有歌詞時才能操作
                    w.export_btn.set_sensitive(can_clear_lyrics);
                    w.export_dropdown.set_sensitive(can_clear_lyrics);
                }
            });
            gtk::glib::ControlFlow::Break
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        // 非 Linux 平台無標題列，忽略所有參數避免 unused variable 警告
        let _ = audio_file_name;
        let _ = lyric_file_name;
        let _ = can_clear_media;
        let _ = can_clear_lyrics;
        let _ = can_load_embedded_lyrics;
    }
    Ok(())
}

/// [Tauri command] 每次復原/重複歷史堆疊改變時，前端呼叫此命令。
/// 會動態重建復原與重複的下拉選單項目清單，
/// 讓使用者可以從標題列直接選擇「復原到某個步驟」。
///
/// 參數說明：
/// - can_undo / can_redo：控制按鈕與下拉箭頭的啟用狀態
/// - undo_list / redo_list：每個步驟的描述文字（由前端 pastActions/futureActions 提供）
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
                    // ── 復原按鈕 ──
                    w.undo_btn.set_sensitive(can_undo);
                    w.undo_dropdown.set_sensitive(can_undo);

                    // 清空舊的復原歷史選單，重新填入最新清單
                    for child in w.undo_menu.children() { w.undo_menu.remove(&child); }
                    let webview = window.clone();
                    for (i, label) in undo_list.iter().enumerate() {
                        let steps = i + 1; // steps=1 表示復原最近一步，steps=2 表示復原前兩步，依此類推
                        let item = gtk::MenuItem::with_label(&format!("復原到: {}", label));
                        let wv = webview.clone();
                        item.connect_activate(move |_| {
                            // 呼叫前端 AppCommands.undoToSequence(steps) 一次復原多步
                            let _ = wv.eval(&format!(
                                "window.AppCommands && window.AppCommands.undoToSequence && window.AppCommands.undoToSequence({})",
                                steps
                            ));
                        });
                        w.undo_menu.append(&item);
                    }
                    w.undo_menu.show_all();

                    // ── 重複按鈕 ──
                    w.redo_btn.set_sensitive(can_redo);
                    w.redo_dropdown.set_sensitive(can_redo);

                    // 清空舊的重複歷史選單，重新填入最新清單
                    for child in w.redo_menu.children() { w.redo_menu.remove(&child); }
                    let webview = window.clone();
                    for (i, label) in redo_list.iter().enumerate() {
                        let steps = i + 1;
                        let item = gtk::MenuItem::with_label(&format!("重複到: {}", label));
                        let wv = webview.clone();
                        item.connect_activate(move |_| {
                            // 呼叫前端 AppCommands.redoToSequence(steps) 一次重複多步
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

/// 建立一個帶圖示的 GtkImageMenuItem 並連結點擊事件到前端 AppCommands 的指定方法。
///
/// # 參數
/// - `label`：選單項目的顯示文字
/// - `icon_name`：GTK symbolic icon 名稱（如 `"edit-clear-symbolic"`），None 則不顯示圖示
/// - `danger`：若為 true，以紅色（`#f87171`，對應前端 `text-red-400`）顯示圖示與文字
/// - `webview`：Tauri WebviewWindow，用來呼叫 JS eval
/// - `js_command`：要執行的完整 JS 表達式（AppCommands.xxx()）
///
/// # 回傳
/// 建立好且已連結 connect_activate 的 GtkMenuItem（內含 GtkBox 組合圖示與文字）
#[cfg(target_os = "linux")]
fn make_menu_item(
    label: &str,
    icon_name: Option<&str>,
    danger: bool,
    webview: tauri::WebviewWindow,
    js_command: &'static str,
) -> gtk::MenuItem {
    use gtk::prelude::*;
    let item = gtk::MenuItem::new();

    // 用 HBox 組合圖示與文字，對應前端 flex items-center gap-2 的排版方式
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if let Some(icon) = icon_name {
        let image = gtk::Image::from_icon_name(Some(icon), gtk::IconSize::Menu);
        // [DANGER] 對應前端 text-red-400：將 symbolic icon 強制渲染為紅色
        if danger {
            let css = gtk::CssProvider::new();
            let _ = css.load_from_data(b"image { color: #f87171; }");
            image.style_context().add_provider(&css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        hbox.pack_start(&image, false, false, 0);
    }
    let lbl = gtk::Label::new(Some(label));
    lbl.set_xalign(0.0); // 靠左對齊，與 GTK 原生選單一致
    // [DANGER] 對應前端 text-red-400 (#f87171 ≈ Tailwind red-400）
    if danger {
        let css = gtk::CssProvider::new();
        let _ = css.load_from_data(b"label { color: #f87171; }");
        lbl.style_context().add_provider(&css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    hbox.pack_start(&lbl, true, true, 0);
    item.add(&hbox);

    item.connect_activate(move |_| {
        // eval 會在 WebView 內執行 JS，透過 window.AppCommands 橋接前端邏輯
        let _ = webview.eval(js_command);
    });
    item
}

/// 建立一個帶圖示+文字標籤的 GtkButton，對應前端按鈕的 flex items-center gap-2 排版。
///
/// # 參數
/// - `label`：按鈕顯示文字
/// - `icon_name`：GTK symbolic icon 名稱
///
/// # 回傳
/// 設定好外觀的 GtkButton（尚未連結 click 事件）
#[cfg(target_os = "linux")]
fn make_icon_button(label: &str, icon_name: &str) -> gtk::Button {
    use gtk::prelude::*;
    let btn = gtk::Button::new();
    let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let image = gtk::Image::from_icon_name(Some(icon_name), gtk::IconSize::SmallToolbar);
    let lbl = gtk::Label::new(Some(label));
    hbox.pack_start(&image, false, false, 0);
    hbox.pack_start(&lbl, false, false, 0);
    hbox.show_all();
    btn.add(&hbox);
    btn
}

/// 初始化 Linux 專用的 GTK3 HeaderBar 標題列。
/// 這個函式只在 Linux 平台編譯，取代 Tauri 預設的視窗標題列，
/// 實現自訂按鈕群組、拖曳、雙擊最大化等行為。
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

            // ══════════════════════════════════════════════════
            // 1. 媒體群組（HeaderBar 左側第一組，使用 "linked" 樣式讓按鈕緊靠）
            // ══════════════════════════════════════════════════
            let media_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            media_box.style_context().add_class("linked"); // GTK "linked" 讓相鄰按鈕共用邊框

            // 主按鈕：開啟系統檔案選擇器載入媒體
            // 對應 TopToolbar.tsx: <Music className="w-3.5 h-3.5 text-blue-400" /> + {i18n.loadMedia}
            let load_media_btn = make_icon_button("載入媒體", "audio-x-generic-symbolic");
            let webview_clone = webview_window.clone();
            load_media_btn.connect_clicked(move |_| {
                // 透過 eval 呼叫前端 AppCommands.loadMedia()，觸發檔案選擇器
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadMedia && window.AppCommands.loadMedia()");
            });
            media_box.pack_start(&load_media_btn, false, false, 0);

            // 下拉選單：「清除媒體」— 由 make_menu_item 統一建立並連結 JS 呼叫
            // 對應 TopToolbar.tsx: <X className="w-3.5 h-3.5" /> + {i18n.clearMedia}（紅色）
            // 此項目的 sensitive 狀態由 on_app_state_changed 控制（有媒體才啟用）
            let media_menu = gtk::Menu::new();
            let clear_media_item = make_menu_item(
                "清除媒體",
                Some("edit-delete-symbolic"), // 對應前端 <X /> 圖示（刪除/清除語意）
                true,  // danger=true → 紅色文字/圖示，對應前端 text-red-400
                webview_window.clone(),
                "window.AppCommands && window.AppCommands.clearMedia && window.AppCommands.clearMedia()",
            );
            media_menu.append(&clear_media_item);
            media_menu.show_all();

            // MenuButton（倒三角箭頭）：點擊展開上方的 GtkMenu
            let media_dropdown = gtk::MenuButton::new();
            media_dropdown.set_popup(Some(&media_menu));
            media_dropdown.set_tooltip_text(Some("媒體選項"));
            media_box.pack_start(&media_dropdown, false, false, 0);

            // 將整個媒體群組從左側加入 HeaderBar
            header_bar.pack_start(&media_box);

            // Separator 1
            let sep1 = gtk::Separator::new(gtk::Orientation::Vertical);
            header_bar.pack_start(&sep1);

            // ══════════════════════════════════════════════════
            // 2. 歌詞群組（HeaderBar 左側第二組）
            // ══════════════════════════════════════════════════
            let lyrics_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            lyrics_box.style_context().add_class("linked");

            // 主按鈕：開啟系統檔案選擇器載入 .lrc 歌詞
            // 對應 TopToolbar.tsx: <FileText className="w-3.5 h-3.5 text-purple-400" /> + {i18n.loadLyrics}
            let load_lyrics_btn = make_icon_button("載入歌詞", "document-open-symbolic");
            let webview_clone = webview_window.clone();
            load_lyrics_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.loadLyrics && window.AppCommands.loadLyrics()");
            });
            lyrics_box.pack_start(&load_lyrics_btn, false, false, 0);

            // 下拉選單（順序與 TopToolbar.tsx 對齊）
            let lyrics_menu = gtk::Menu::new();

            // 項目 1：載入歌詞檔案（與主按鈕功能相同，方便從下拉操作）
            // 對應 TopToolbar.tsx 下拉第一項：{i18n.loadLyrics}
            let load_lyrics_menu_item = make_menu_item(
                "載入歌詞檔案",
                Some("document-open-symbolic"), // 與主按鈕圖示一致
                false,
                webview_window.clone(),
                "window.AppCommands && window.AppCommands.loadLyrics && window.AppCommands.loadLyrics()",
            );
            lyrics_menu.append(&load_lyrics_menu_item);

            // 項目 2：從媒體的 ID3/Vorbis tag 載入內嵌歌詞
            // 對應 TopToolbar.tsx: {i18n.loadEmbeddedLyrics}
            // sensitive 由 on_app_state_changed 控制（媒體 tag 有歌詞才啟用）
            let load_embedded_item = make_menu_item(
                "載入內嵌標籤",
                Some("emblem-music-symbolic"), // 代表「內嵌於媒體中」的標籤語意
                false,
                webview_window.clone(),
                "window.AppCommands && window.AppCommands.loadEmbeddedLyrics && window.AppCommands.loadEmbeddedLyrics()",
            );
            lyrics_menu.append(&load_embedded_item);

            // 水平分隔線，對應前端 <div className="h-px bg-[var(--app-border-base)]" />
            let lyrics_sep = gtk::SeparatorMenuItem::new();
            lyrics_menu.append(&lyrics_sep);

            // 項目 3：清除目前已載入的歌詞
            // 對應 TopToolbar.tsx: <X className="w-3.5 h-3.5" /> + {i18n.clearLyrics}（紅色）
            // sensitive 由 on_app_state_changed 控制（有歌詞才啟用）
            let clear_lyrics_item = make_menu_item(
                "清除歌詞",
                Some("edit-delete-symbolic"), // 對應前端 <X /> 圖示
                true,  // danger=true → 紅色文字/圖示，對應前端 text-red-400
                webview_window.clone(),
                "window.AppCommands && window.AppCommands.clearLyrics && window.AppCommands.clearLyrics()",
            );
            lyrics_menu.append(&clear_lyrics_item);
            lyrics_menu.show_all();

            // MenuButton（倒三角箭頭）連結到歌詞下拉選單
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

            // ══════════════════════════════════════════════════
            // 4. 匯出群組（pack_end 放在 HeaderBar 右側）
            // ══════════════════════════════════════════════════
            let export_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            export_box.style_context().add_class("linked");

            // 主按鈕：快速匯出目前格式
            // 對應 TopToolbar.tsx: <Download className="w-4 h-4" /> + {i18n.exportLrc}
            let export_btn = make_icon_button("匯出 .lrc", "document-save-symbolic");
            let webview_clone = webview_window.clone();
            export_btn.connect_clicked(move |_| {
                let _ = webview_clone.eval("window.AppCommands && window.AppCommands.exportCurrent && window.AppCommands.exportCurrent()");
            });
            export_box.pack_start(&export_btn, false, false, 0);

            // 下拉選單：選擇不同的匯出格式
            let export_menu = gtk::Menu::new();

            // 標準 LRC：每行一個時間戳，相容大多數播放器
            // 對應 TopToolbar.tsx 下拉第一項：{i18n.exportStandard}
            let export_standard_item = make_menu_item(
                "標準 LRC (行同步)",
                Some("document-save-symbolic"), // 與主按鈕圖示一致
                false,
                webview_window.clone(),
                "window.AppCommands && window.AppCommands.exportStandard && window.AppCommands.exportStandard()",
            );
            export_menu.append(&export_standard_item);

            // 逐字版 LRC：每個字詞獨立時間戳，供 ESLyric 等進階播放器使用
            // 對應 TopToolbar.tsx 下拉第二項：{i18n.exportEnhanced}
            let export_enhanced_item = make_menu_item(
                "逐字版 LRC (ESLyric - 逐字同步)",
                Some("document-save-as-symbolic"), // save-as 語意更接近「另存為其他格式」
                false,
                webview_window.clone(),
                "window.AppCommands && window.AppCommands.exportEnhanced && window.AppCommands.exportEnhanced()",
            );
            export_menu.append(&export_enhanced_item);
            export_menu.show_all();

            // MenuButton（倒三角箭頭）連結到匯出下拉選單
            let export_dropdown = gtk::MenuButton::new();
            export_dropdown.set_popup(Some(&export_menu));
            export_dropdown.set_tooltip_text(Some("匯出選項"));
            export_box.pack_start(&export_dropdown, false, false, 0);

            // 匯出群組放右側（pack_end 由右往左排）
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

