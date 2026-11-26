//! Dispatch the selected paths to the configured Windows previewer.

use super::*;
use favnyr_core::config::QuickPreviewProvider;
use std::process::Command;

pub(super) fn install(window: &MainWindow, state: &AppState) {
    sync_settings(window, state);
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_quick_preview_provider_changed(move |index| {
            let Some(w) = weak.upgrade() else { return };
            st.persist_config(|cfg| {
                cfg.quick_preview_provider = QuickPreviewProvider::from_index(index)
            });
            sync_settings(&w, &st);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_quick_preview_browse(move || {
            let Some(w) = weak.upgrade() else { return };
            let lang = st.config.borrow().language;
            if let Some(path) = openwith::browse_for_exe(lang) {
                save_program(&w, &st, Some(PathBuf::from(path)));
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_quick_preview_reset(move || {
            if let Some(w) = weak.upgrade() {
                save_program(&w, &st, None);
            }
        });
    }
    let st = state.clone();
    let weak = window.as_weak();
    let launching = Arc::new(AtomicBool::new(false));
    window.on_action_quick_preview(move || {
        let Some(w) = weak.upgrade() else {
            return false;
        };
        let (provider, override_path, lang) = {
            let config = st.config.borrow();
            (
                config.quick_preview_provider,
                program_override(&config).cloned(),
                config.language,
            )
        };
        if provider == QuickPreviewProvider::Disabled {
            return false;
        }
        let mut paths = selected_paths(&st);
        if paths.is_empty() {
            return false;
        }
        if provider == QuickPreviewProvider::QuickLook {
            // QuickLook previews one item. Prefer the keyboard cursor if it
            // belongs to the selection, and otherwise use the first item.
            let cursor = st.with_tabs(|book| book.tabs[book.active].cursor);
            let first = usize::try_from(cursor)
                .ok()
                .and_then(|index| st.active_rows_model().row_data(index))
                .filter(|row| row.selected)
                .map_or_else(|| paths.remove(0), |row| row_path(&row));
            paths = vec![first];
        }
        if launching.swap(true, Ordering::Relaxed) {
            return true;
        }
        let busy = launching.clone();
        let weak_for_worker = w.as_weak();
        if let Err(error) = std::thread::Builder::new()
            .name("favnyr-quick-preview".into())
            .spawn(move || {
                let result = launch(provider, override_path, &paths);
                busy.store(false, Ordering::Relaxed);
                if let Err(key) = result {
                    let _ = weak_for_worker.upgrade_in_event_loop(move |window| {
                        show_notice(&window, i18n::tr(lang, key));
                    });
                }
            })
        {
            launching.store(false, Ordering::Relaxed);
            warn!(%error, "could not start quick preview worker");
            show_notice(&w, i18n::tr(lang, "quick_preview_failed"));
        }
        true
    });
}

fn program_override(config: &Config) -> Option<&PathBuf> {
    match config.quick_preview_provider {
        QuickPreviewProvider::PowerToysPeek => config.peek_executable.as_ref(),
        QuickPreviewProvider::QuickLook => config.quicklook_executable.as_ref(),
        QuickPreviewProvider::Disabled => None,
    }
}

fn sync_settings(window: &MainWindow, state: &AppState) {
    let config = state.config.borrow();
    window.set_quick_preview_provider(config.quick_preview_provider.index());
    window.set_quick_preview_program(
        program_override(&config).map_or_else(SharedString::default, |path| {
            path.to_string_lossy().as_ref().into()
        }),
    );
}

fn save_program(window: &MainWindow, state: &AppState, path: Option<PathBuf>) {
    state.persist_config(|cfg| match cfg.quick_preview_provider {
        QuickPreviewProvider::PowerToysPeek => cfg.peek_executable = path,
        QuickPreviewProvider::QuickLook => cfg.quicklook_executable = path,
        QuickPreviewProvider::Disabled => {}
    });
    sync_settings(window, state);
}

fn launch(
    provider: QuickPreviewProvider,
    override_path: Option<PathBuf>,
    paths: &[PathBuf],
) -> Result<(), &'static str> {
    let executable = override_path
        .or_else(|| find_program(provider))
        .filter(|path| path.is_absolute() && path.is_file())
        .ok_or("quick_preview_missing")?;
    Command::new(executable)
        .args(paths)
        .spawn()
        .map_err(|error| {
            warn!(%error, ?provider, "quick preview failed to launch");
            "quick_preview_failed"
        })?;
    Ok(())
}

fn find_program(provider: QuickPreviewProvider) -> Option<PathBuf> {
    let (per_user, installed) = match provider {
        QuickPreviewProvider::PowerToysPeek => {
            let exe = r"PowerToys\WinUI3Apps\PowerToys.Peek.UI.exe";
            (exe, exe)
        }
        QuickPreviewProvider::QuickLook => (
            r"Programs\QuickLook\QuickLook.exe",
            r"QuickLook\QuickLook.exe",
        ),
        QuickPreviewProvider::Disabled => return None,
    };
    [
        ("LOCALAPPDATA", per_user),
        ("ProgramW6432", installed),
        ("ProgramFiles", installed),
        ("ProgramFiles(x86)", installed),
    ]
    .into_iter()
    .filter_map(|(root, relative)| {
        std::env::var_os(root).map(|path| PathBuf::from(path).join(relative))
    })
    .find(|path| path.is_absolute() && path.is_file())
}
