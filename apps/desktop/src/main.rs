#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("mcp") => {
            if let Err(e) = micrack_desktop_lib::mcp::run() {
                eprintln!("micrack mcp: {e}");
                std::process::exit(1);
            }
        }
        Some("mcp-install") => {
            let exe = std::env::current_exe().unwrap_or_default();
            let snippet = serde_json::json!({
                "mcpServers": {"micrack": {"command": exe, "args": ["mcp"]}}
            });
            println!("Register with Claude Code:\n  claude mcp add micrack -- \"{}\" mcp\n", exe.display());
            println!("Or merge into .mcp.json / Codex mcp_servers config:\n{}", serde_json::to_string_pretty(&snippet).unwrap());
        }
        _ => micrack_desktop_lib::run(),
    }
}