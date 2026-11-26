//! Resolve Windows Shell icons for the current viewport without blocking Slint.

use super::*;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Source {
    Extension(String),
    File(PathBuf, u64),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    source: Source,
    large: bool,
}

impl Key {
    fn for_row(row: &FileRow, large: bool) -> Option<Self> {
        let ext = row.ext.as_str();
        if row.is_dir || ext.is_empty() {
            return None;
        }
        let source = if !row.content_remote && (has_own_icon(ext) || ext == "lnk") {
            Source::File(row_path(row), 0)
        } else {
            Source::Extension(ext.to_owned())
        };
        Some(Self { source, large })
    }
}

type Pixels = (Vec<u8>, u32, u32);

#[derive(Default)]
struct Resolved {
    pixels: Option<Pixels>,
    link_folder: bool,
    link_preview: Option<(FileKind, PathBuf)>,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Key>,
    in_flight: Option<Key>,
    ready: Vec<(Key, Resolved)>,
    closed: bool,
}

impl Queue {
    fn replace_pending(&mut self, keys: impl IntoIterator<Item = Key>) {
        let mut seen = HashSet::new();
        self.pending = keys
            .into_iter()
            .filter(|key| {
                self.in_flight.as_ref() != Some(key)
                    && !self.ready.iter().any(|(ready, _)| ready == key)
                    && seen.insert(key.clone())
            })
            .collect();
    }
}

#[derive(Default)]
struct Cache {
    images: HashMap<Key, (Image, bool)>,
    link_previews: HashMap<PathBuf, (FileKind, PathBuf)>,
    file_generations: HashMap<PathBuf, u64>,
}

#[derive(Default)]
pub(super) struct RowIcons {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    cache: RefCell<Cache>,
}

impl RowIcons {
    pub(super) fn install(window: &MainWindow, state: &AppState) {
        let worker = state.row_icons.queue.clone();
        let weak = window.as_weak();
        if let Err(error) = std::thread::Builder::new()
            .name("favnyr-row-icons".into())
            .spawn(move || work(worker, weak))
        {
            warn!(%error, "could not start row icon worker");
        }
        let st = state.clone();
        window.on_row_icons_ready(move || {
            if st.row_icons.drain() {
                request_thumbnails(&st);
            }
            Self::refresh(&st);
        });
    }

    pub(super) fn shortcut_preview(&self, path: &Path) -> Option<(FileKind, PathBuf)> {
        self.cache.borrow().link_previews.get(path).cloned()
    }

    pub(super) fn invalidate_paths(&self, paths: &HashSet<PathBuf>) {
        let mut cache = self.cache.borrow_mut();
        for path in paths {
            let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if ext.eq_ignore_ascii_case("lnk") || has_own_icon(&ext.to_ascii_lowercase()) {
                let generation = cache.file_generations.entry(path.clone()).or_default();
                *generation = generation.wrapping_add(1);
                cache.link_previews.remove(path);
            }
        }
        cache.images.retain(|key, _| match &key.source {
            Source::File(path, _) => !paths.contains(path),
            Source::Extension(_) => true,
        });
    }

    /// Pulls back owned pixels. Slint images and the row model stay on the UI thread.
    fn drain(&self) -> bool {
        let ready = std::mem::take(&mut self.queue.0.lock().unwrap().ready);
        let mut cache = self.cache.borrow_mut();
        let mut new_previews = false;
        for (key, result) in ready {
            if let Source::File(path, generation) = &key.source
                && cache.file_generations.get(path).copied().unwrap_or(0) != *generation
            {
                continue;
            }
            let image = result.pixels.map_or_else(Image::default, |(rgba, w, h)| {
                Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                    &rgba, w, h,
                ))
            });
            if let (Source::File(path, _), Some(preview)) = (&key.source, result.link_preview)
                && cache.link_previews.get(path) != Some(&preview)
            {
                cache.link_previews.insert(path.clone(), preview);
                new_previews = true;
            }
            cache.images.insert(key, (image, result.link_folder));
        }
        new_previews
    }

    pub(super) fn refresh(state: &AppState) {
        let icons = &state.row_icons;
        let (Ok(panels), Ok(compact), Ok(cache)) = (
            state.panels.try_borrow(),
            state.config.try_borrow(),
            icons.cache.try_borrow(),
        ) else {
            return;
        };
        let compact = compact.compact_icon_rows_in_preview;
        let mut visible_jobs = Vec::new();
        let mut overscan_jobs = Vec::new();
        for panel in panels.iter() {
            if panel.tabs.group_page().is_some() {
                continue;
            }
            let model = &panel.rows_model;
            let tab = &panel.tabs.tabs[panel.tabs.active];
            let grid = tab.item_arrangement == ItemArrangement::Grid;
            let top = panel.viewport_top.get();
            let visible = arranged_model_geometry(&**model, panel.item_grid.get())
                .visible_range(top, top + panel.viewport_height.get().max(1.0));
            for index in panel.rendered_first.get()..panel.rendered_end.get() {
                let Some(mut row) = model.row_data(index) else {
                    continue;
                };
                let large = grid || tab.preview && (!compact || row.preview_capable);
                let Some(mut key) = Key::for_row(&row, large) else {
                    continue;
                };
                if let Source::File(path, generation) = &mut key.source {
                    *generation = cache.file_generations.get(path).copied().unwrap_or(0);
                }
                if let Some((image, link_folder)) = cache.images.get(&key) {
                    if (row.app_icon.size().width == 0 && image.size().width != 0)
                        || row.link_folder != *link_folder
                    {
                        row.app_icon = image.clone();
                        row.link_folder = *link_folder;
                        model.set_row_data(index, row);
                    }
                } else if visible.contains(&index) {
                    visible_jobs.push(key);
                } else {
                    overscan_jobs.push(key);
                }
            }
        }
        let (queue, wake) = &*icons.queue;
        let mut queue = queue.lock().unwrap();
        queue.replace_pending(visible_jobs.into_iter().chain(overscan_jobs));
        if !queue.pending.is_empty() {
            wake.notify_one();
        }
    }
}

impl Drop for RowIcons {
    fn drop(&mut self) {
        let (queue, wake) = &*self.queue;
        queue.lock().unwrap().closed = true;
        wake.notify_one();
    }
}

fn work(worker: Arc<(Mutex<Queue>, Condvar)>, window: slint::Weak<MainWindow>) {
    let Some(_apartment) = crate::winutil::ComApartment::sta() else {
        warn!("could not initialize COM for row icons");
        return;
    };
    let (queue, wake) = &*worker;
    let mut extensions = HashMap::new();
    loop {
        let key = {
            let mut queue = wake
                .wait_while(queue.lock().unwrap(), |q| q.pending.is_empty() && !q.closed)
                .unwrap();
            if queue.closed {
                return;
            }
            let key = queue.pending.pop_front().unwrap();
            queue.in_flight = Some(key.clone());
            key
        };
        let start = Instant::now();
        let result = resolve(&key, &mut extensions);
        debug!(
            elapsed_ms = start.elapsed().as_millis() as u64,
            "row icon resolved"
        );
        let publish = {
            let mut queue = queue.lock().unwrap();
            queue.in_flight = None;
            let publish = queue.ready.is_empty();
            queue.ready.push((key, result));
            publish
        };
        if publish
            && window
                .upgrade_in_event_loop(|window| window.invoke_row_icons_ready())
                .is_err()
        {
            return;
        }
    }
}

fn resolve(key: &Key, extensions: &mut HashMap<(String, bool), Option<Pixels>>) -> Resolved {
    let mut result = Resolved::default();
    let mut extension_icon = |ext: &str| {
        extensions
            .entry((ext.to_owned(), key.large))
            .or_insert_with(|| openwith::icon_rgba_for_ext(ext, key.large))
            .clone()
    };
    match &key.source {
        Source::Extension(ext) => result.pixels = extension_icon(ext),
        Source::File(path, _) => {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let ext = ops::ext_of(&name);
            // A listed file may have become a cloud placeholder since the
            // scan. Recheck immediately before any automatic Shell read.
            if !crate::cloud_content_guard::may_read_automatically(path) {
                result.pixels = extension_icon(&ext);
                return result;
            }
            if ext == "lnk" {
                let Some(target) = cached_link_target(path) else {
                    result.pixels = extension_icon(&ext);
                    return result;
                };
                if target.is_dir {
                    result.link_folder = true;
                    return result;
                }
                if crate::cloud_content_guard::may_read_automatically(&target.path) {
                    let kind = if rfs::ambiguous::rule_for_extension(&target.ext).is_some() {
                        Some(FileKind::File)
                    } else {
                        thumbnail_kind_for_row(
                            rfs::classify_kind(Some(&target.ext), false).as_i32(),
                            &target.ext,
                        )
                        .map(|(kind, _)| kind)
                    };
                    result.link_preview = kind.map(|kind| (kind, target.path));
                    result.pixels =
                        openwith::icon_rgba_for_path(&path.to_string_lossy(), key.large);
                }
                if result.pixels.is_none() {
                    result.pixels = extension_icon(&target.ext);
                }
                return result;
            }
            result.pixels = openwith::icon_rgba_for_path(&path.to_string_lossy(), key.large)
                .or_else(|| extension_icon(&ext));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_shortcut_uses_the_extension_icon_and_old_targets_are_discarded() {
        let mut row = FileRow {
            path: r"C:\folder".into(),
            name: "preview.lnk".into(),
            ext: "lnk".into(),
            content_remote: true,
            ..FileRow::default()
        };
        assert_eq!(Key::for_row(&row, false), Some(extension("lnk")));

        row.content_remote = false;
        let previous = Key::for_row(&row, false).unwrap();
        let icons = RowIcons::default();
        let path = row_path(&row);
        icons.invalidate_paths(&HashSet::from([path.clone()]));
        icons.queue.0.lock().unwrap().ready.push((
            previous,
            Resolved {
                link_preview: Some((FileKind::Image, path.with_extension("png"))),
                ..Resolved::default()
            },
        ));

        assert!(!icons.drain());
        assert!(icons.shortcut_preview(&path).is_none());
    }

    fn extension(ext: &str) -> Key {
        Key {
            source: Source::Extension(ext.into()),
            large: false,
        }
    }

    #[test]
    fn viewport_replaces_pending_icons_without_repeating_active_or_completed_work() {
        let stale = extension("zip");
        let active = extension("pdf");
        let ready = extension("jpg");
        let visible = extension("txt");
        let overscan = extension("csv");
        let mut queue = Queue {
            pending: VecDeque::from([stale]),
            in_flight: Some(active.clone()),
            ready: vec![(ready.clone(), Resolved::default())],
            ..Queue::default()
        };
        queue.replace_pending([
            visible.clone(),
            active,
            ready,
            visible.clone(),
            overscan.clone(),
        ]);
        assert_eq!(queue.pending, VecDeque::from([visible, overscan]));
    }

    #[test]
    fn a_late_icon_matches_the_current_path_and_size() {
        crate::test_support::run(|| {
            let state = AppState::new(Config::default());
            state.config.borrow_mut().compact_icon_rows_in_preview = true;
            let row = FileRow {
                path: r"C:\new".into(),
                name: "setup.exe".into(),
                ext: "exe".into(),
                ..FileRow::default()
            };
            let panels = state.panels.borrow();
            let panel = &panels[0];
            let model = panel.rows_model.clone();
            panel.rendered_end.set(1);
            model.set_vec(vec![row.clone()]);
            drop(panels);

            let small = Key::for_row(&row, false).unwrap();
            let wrong_size = Key::for_row(&row, true).unwrap();
            let wrong_path = Key {
                source: Source::File(PathBuf::from(r"C:\old\setup.exe"), 0),
                large: false,
            };
            let image = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::new(1, 1));
            state.row_icons.cache.borrow_mut().images.extend([
                (wrong_path, (image.clone(), false)),
                (wrong_size, (image.clone(), false)),
            ]);
            RowIcons::refresh(&state);
            assert_eq!(model.row_data(0).unwrap().app_icon.size().width, 0);

            state
                .row_icons
                .cache
                .borrow_mut()
                .images
                .insert(small, (image, false));
            RowIcons::refresh(&state);
            assert_eq!(model.row_data(0).unwrap().app_icon.size().width, 1);
        });
    }
}
