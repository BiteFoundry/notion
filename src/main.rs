use std::ffi::OsStr;

use anyhow::Result;

fn needs_multi_thread_runtime<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut iter = args.into_iter();
    let _bin = iter.next();
    while let Some(arg) = iter.next() {
        let s = arg.as_ref().to_str().unwrap_or("");
        if s == "--workspace" || s == "-w" {
            let _ = iter.next();
            continue;
        }
        if s.starts_with("--workspace=") {
            continue;
        }
        return match s {
            "tui" | "serve" | "api" | "mcp" | "mcp-server" => true,
            "daemon" => matches!(
                iter.next().as_ref().and_then(|v| v.as_ref().to_str()),
                Some("run")
            ),
            _ => false,
        };
    }
    true
}

fn main() -> Result<()> {
    let handle = std::thread::Builder::new()
        .name("notion-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_main)?;
    match handle.join() {
        Ok(res) => res,
        Err(panic_payload) => std::panic::resume_unwind(panic_payload),
    }
}

fn run_main() -> Result<()> {
    if let Some(res) = notion_cli::cli::try_fast_help_or_version() {
        return res;
    }

    if needs_multi_thread_runtime(std::env::args_os()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let _ = rustls::crypto::ring::default_provider().install_default();
            tokio::task::spawn_blocking(notion_cli::updater::cleanup_stale_old_binary);
            notion_cli::cli::run_from_env().await
        })
    } else {
        let _ = rustls::crypto::ring::default_provider().install_default();
        notion_cli::updater::cleanup_stale_old_binary();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(notion_cli::cli::run_from_env())
    }
}

#[cfg(test)]
mod tests {
    use super::needs_multi_thread_runtime;

    #[test]
    fn detects_multi_thread_commands_only_for_tui_serve_mcp_and_daemon_run() {
        assert!(needs_multi_thread_runtime(["notion"]));
        assert!(needs_multi_thread_runtime(["notion", "--workspace", "acme"]));
        assert!(needs_multi_thread_runtime(["notion", "tui"]));
        assert!(needs_multi_thread_runtime(["notion", "serve"]));
        assert!(needs_multi_thread_runtime(["notion", "api"]));
        assert!(needs_multi_thread_runtime(["notion", "mcp"]));
        assert!(needs_multi_thread_runtime(["notion", "daemon", "run"]));

        for sub in ["status", "stop", "start", "restart"] {
            assert!(!needs_multi_thread_runtime(["notion", "daemon", sub]));
        }

        for cmd in [
            "login",
            "logout",
            "whoami",
            "workspaces",
            "search",
            "pages",
            "blocks",
            "db",
            "comments",
            "users",
            "download",
            "sync",
            "doctor",
            "update",
            "--help",
            "--version",
        ] {
            assert!(!needs_multi_thread_runtime(["notion", cmd]), "{cmd}");
        }
    }
}
