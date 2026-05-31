use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use tauri::Manager;

// ── Python server lifecycle ────────────────────────────────────────────

struct PythonServer(Mutex<Option<Child>>);

/// Simple home dir helper (avoids pulling in the `dirs` crate).
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Find the Hermes-agent venv Python interpreter.
fn find_python() -> Result<PathBuf, String> {
    let home = home_dir().unwrap_or_default();
    let candidates = [
        // Hermes-agent venv (preferred — has hermes module)
        home.join(".hermes/hermes-agent/venv/bin/python3"),
        home.join(".hermes/hermes-agent/venv/bin/python"),
        // System Python (fallback)
        PathBuf::from("/usr/bin/python3"),
        PathBuf::from("/usr/bin/python"),
    ];

    for c in &candidates {
        if c.exists() {
            println!("[hermes-desktop] Using Python: {:?}", c);
            return Ok(c.clone());
        }
    }

    Err("No Python interpreter found".into())
}

/// Resolve the Hermes-agent Python path (adds to PYTHONPATH so `import hermes` works).
fn hermes_agent_path() -> PathBuf {
    let home = home_dir().unwrap_or_default();
    home.join(".hermes/hermes-agent")
}

/// Find server.py and return the directory it lives in.
fn find_server_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // Resource dir first (production / bundled)
    if let Ok(res) = app.path().resource_dir() {
        let bundled = res.join("server.py");
        if bundled.exists() {
            println!("[hermes-desktop] Using bundled server: {:?}", bundled);
            return Ok(res);
        }
    }

    // Fallback: search upward from cwd (dev mode)
    let mut cwd = std::env::current_dir().map_err(|e| format!("cwd error: {}", e))?;
    loop {
        if cwd.join("server.py").exists() {
            println!("[hermes-desktop] Using dev server: {:?}", cwd);
            return Ok(cwd);
        }
        if !cwd.pop() {
            break;
        }
    }

    Err("server.py not found — run from the hermes-webui repo root".into())
}

/// Start the Python backend server using the Hermes-venv Python.
fn start_python_server(python: &PathBuf, server_dir: &PathBuf) -> Result<Child, String> {
    let server_py = server_dir.join("server.py");
    println!(
        "[hermes-desktop] Starting server: {} with Python: {}",
        server_py.display(),
        python.display()
    );

    let hermes_home = home_dir().unwrap_or_default().join(".hermes");
    let agent_path = hermes_agent_path();

    // Build PYTHONPATH so `import hermes` works from the venv
    let pythonpath = std::env::var("PYTHONPATH").unwrap_or_default();
    let augmented_path = if pythonpath.is_empty() {
        agent_path.to_string_lossy().to_string()
    } else {
        format!("{}:{}", agent_path.to_string_lossy(), pythonpath)
    };

    let child = Command::new(python)
        .arg(&server_py)
        .env("PORT", "8787")
        .env("HOST", "127.0.0.1")
        .env("HERMES_WEBUI_PORT", "8787")
        .env("HERMES_HOME", hermes_home)
        .env("PYTHONPATH", augmented_path)
        .current_dir(server_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start Python server: {}", e))?;

    Ok(child)
}

/// Poll TCP port 8787 until HTTP 200.
fn wait_for_server(timeout_secs: u64) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed().as_secs() > timeout_secs {
            return Err(format!("Server did not start within {}s", timeout_secs));
        }
        if TcpStream::connect_timeout(
            &"127.0.0.1:8787".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
        {
            std::thread::sleep(Duration::from_millis(100));
            if let Ok(mut stream) = TcpStream::connect_timeout(
                &"127.0.0.1:8787".parse().unwrap(),
                Duration::from_millis(300),
            ) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = stream.write_all(b"GET /health HTTP/1.0\r\n\r\n");
                let mut buf = [0u8; 256];
                if let Ok(n) = stream.read(&mut buf) {
                    let resp = String::from_utf8_lossy(&buf[..n]);
                    if resp.contains("200") {
                        println!("[hermes-desktop] Server is ready!");
                        return Ok(());
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ── Application entry ───────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // Find Python and server dir
            let python = find_python().expect("Failed to locate Python interpreter");
            let server_dir = find_server_dir(&handle).expect("Failed to locate server.py");
            let child = start_python_server(&python, &server_dir)
                .expect("Failed to start Python server");

            app.manage(PythonServer(Mutex::new(Some(child))));

            // Wait for server in background, then navigate
            std::thread::spawn(move || match wait_for_server(30) {
                Ok(()) => {
                    println!("[hermes-desktop] Server ready — navigating to web UI");
                    if let Some(window) = handle.get_webview_window("main") {
                        let url: tauri::Url =
                            "http://127.0.0.1:8787".parse().expect("Invalid URL");
                        let _ = window.navigate(url);
                    }
                }
                Err(e) => eprintln!("[hermes-desktop] {e}"),
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(state) = window.try_state::<PythonServer>() {
                    if let Ok(mut guard) = state.0.lock() {
                        if let Some(mut child) = guard.take() {
                            println!("[hermes-desktop] Shutting down Python server...");
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("Error running Hermes Desktop");
}
