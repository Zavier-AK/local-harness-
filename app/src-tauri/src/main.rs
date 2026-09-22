// Keep the console window off on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Claude Code runs this same binary as a hook when a native subagent needs a worktree
    // (see `harness_core::hooks`). That call must never open a window.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(harness_app_lib::HOOK_ARG) {
        std::process::exit(harness_app_lib::run_hook(args.get(2).map(String::as_str)));
    }
    harness_app_lib::run()
}
