fn main() {
    use std::io::IsTerminal;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("--trusted-install-helper")
    ) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Err(error) = bedit::installer::main(&args[1..]) {
            eprintln!("bedit: trusted installer helper failed: {error}");
            std::process::exit(2);
        } else {
            return;
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            eprintln!("bedit: secure installation is supported only on Linux");
            std::process::exit(2);
        }
    }
    if matches!(args.as_slice(), [arg] if arg == "--version" || arg == "-V") {
        println!("bedit {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if matches!(args.as_slice(), [arg] if arg == "--editor-registry") {
        for alias in bedit::editor::EDITOR_ALIASES
            .iter()
            .filter(|alias| alias.supported)
        {
            println!("{} {}", alias.name, alias.named_wrapper);
        }
        return;
    }
    let code =
        if args.is_empty() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            bedit::tui::main()
        } else {
            bedit::cli::main(args)
        };
    std::process::exit(code);
}
