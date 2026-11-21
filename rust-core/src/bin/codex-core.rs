fn main() {
    #[cfg(feature = "benchmark")]
    if std::env::var_os("MSM_DISK_CACHE_ROOT").is_none() {
        eprintln!("codex-core: benchmark builds require MSM_DISK_CACHE_ROOT");
        std::process::exit(2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--mcp-version") if args.len() == 1 => {
            println!("{}", codex_storage_core::mcp::server_version());
            Ok(())
        }
        Some("--mcp") => codex_storage_core::mcp::run(&args[1..]),
        Some("--mcp-worker") => codex_storage_core::mcp::run_worker(&args[1..]),
        None => codex_storage_core::rpc::run(),
        _ => Err("Usage: codex-core [--mcp [--allow-cleanup] [--shared-cache]]".into()),
    };
    if let Err(error) = result {
        eprintln!("codex-core: {error}");
        std::process::exit(1);
    }
}
