use std::path::PathBuf;

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    rt.block_on(async {
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);

        println!("[terminal-server] starting with config_dir={config_dir:?}");

        if let Err(e) =
            claude_code_desktop_lib::terminal_websocket::start_terminal_websocket_server(config_dir)
                .await
        {
            eprintln!("[terminal-server] failed to start: {e}");
            std::process::exit(1);
        }

        println!("[terminal-server] running. Press Ctrl+C to stop.");

        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for Ctrl+C");
        println!("[terminal-server] shutting down.");
    });
}
