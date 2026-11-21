use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use ksni::blocking::TrayMethods as KsniTrayMethods;

use crate::library::{self, Track};
use crate::player::{PlaybackState, Player};

const TICK_INTERVAL: Duration = Duration::from_millis(100);
const SEEK_DEBOUNCE: Duration = Duration::from_millis(250);
const SEEK_RESTART_THRESHOLD: Duration = Duration::from_secs(3);
const STACK_EMPTY: &str = "empty";
const STACK_LOADING: &str = "loading";
const STACK_LIST: &str = "list";

const SPEEDS: [f32; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

type RowMap = Rc<RefCell<Vec<(PathBuf, gtk::Box)>>>;
type RowCell = Rc<RefCell<Option<Rc<Track>>>>;
type RowCellMap = Rc<RefCell<HashMap<usize, RowCell>>>;

struct LyricLine {
    time: Option<Duration>,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepeatMode {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Lang {
    #[default]
    Zh,
    En,
}

impl Lang {
    fn t(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Lang::Zh => zh,
            Lang::En => en,
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        match code.trim() {
            "zh" => Some(Lang::Zh),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum EmptyKind {
    #[default]
    Welcome,
    NoMusic,
    ScanFailed,
}

#[derive(Default)]
struct SavedState {
    folder: Option<PathBuf>,
    volume: Option<f32>,
    favorites: HashSet<PathBuf>,
    removed: HashSet<PathBuf>,
    lang: Option<Lang>,
}

struct LibraryState {
    favorites: RefCell<HashSet<PathBuf>>,
    removed: RefCell<HashSet<PathBuf>>,
    favorites_only: Cell<bool>,
    lang: Cell<Lang>,
}

pub fn build(app: &gtk::Application) {
    let saved = load_config();
    let library_state = Rc::new(LibraryState {
        favorites: RefCell::new(saved.favorites.clone()),
        removed: RefCell::new(saved.removed.clone()),
        favorites_only: Cell::new(false),
        lang: Cell::new(saved.lang.unwrap_or_default()),
    });
    let lang = library_state.lang.get();

    // ---------- 顶栏 ----------
    let header = gtk::HeaderBar::new();
    // 只保留下最小化和关闭按钮，去掉最大化
    header.set_decoration_layout(Some(":minimize,close"));

    let brand = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    brand.append(&logo_widget(28));
    let app_title = gtk::Label::new(Some("RustMusic"));
    app_title.add_css_class("app-title");
    brand.append(&app_title);

    let search = gtk::SearchEntry::builder()
        .placeholder_text(lang.t("搜索歌曲、艺术家或专辑", "Search songs, artists or albums"))
        .width_request(360)
        .build();
    search.add_css_class("search-entry");

    let header_left = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    header_left.set_margin_start(6);
    header_left.append(&brand);
    header_left.append(&search);

    let settings_btn = gtk::Button::from_icon_name("emblem-system-symbolic");
    settings_btn.set_tooltip_text(Some(lang.t("设置 (Ctrl+,)", "Settings (Ctrl+,)")));
    settings_btn.add_css_class("flat");
    settings_btn.add_css_class("help-button");

    let favorites_icon = gtk::Image::from_icon_name("non-starred-symbolic");
    favorites_icon.set_pixel_size(16);
    let favorites_label = gtk::Label::new(Some(lang.t("收藏夹", "Favorites")));
    let favorites_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    favorites_content.append(&favorites_icon);
    favorites_content.append(&favorites_label);
    let favorites_btn = gtk::ToggleButton::builder()
        .child(&favorites_content)
        .tooltip_text(lang.t("只看收藏的歌曲", "Show favorites only"))
        .css_classes(["flat", "favorites-toggle"])
        .build();

    let import_label = gtk::Label::new(Some(lang.t("导入文件夹", "Import Folder")));
    let import_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    import_content.append(&gtk::Image::from_icon_name("folder-open-symbolic"));
    import_content.append(&import_label);
    let import_btn = gtk::Button::builder()
        .child(&import_content)
        .tooltip_text(lang.t(
            "扫描一个文件夹并导入其中的音乐",
            "Scan a folder and import its music",
        ))
        .css_classes(["suggested-action", "import-button"])
        .build();
    import_btn.set_margin_end(6);

    let header_end = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header_end.append(&settings_btn);
    header_end.append(&favorites_btn);
    header_end.append(&import_btn);

    header.pack_start(&header_left);
    header.pack_end(&header_end);
    header.set_title_widget(Some(&gtk::Label::new(None)));

    // ---------- 空状态页 ----------
    let empty_icon = logo_widget(104);
    empty_icon.set_margin_bottom(2);

    let empty_title = gtk::Label::new(Some(lang.t("还没有音乐", "No music yet")));
    empty_title.add_css_class("empty-title");

    let empty_sub = gtk::Label::new(Some(lang.t(
        "点击右上角「导入文件夹」，或直接选择音乐目录",
        "Use “Import Folder” in the top-right, or choose a music folder",
    )));
    empty_sub.add_css_class("empty-sub");

    let pick_btn = gtk::Button::with_label(lang.t("选择音乐文件夹", "Choose Music Folder"));
    pick_btn.add_css_class("suggested-action");
    pick_btn.add_css_class("empty-action");
    pick_btn.set_halign(gtk::Align::Center);
    pick_btn.set_margin_top(8);

    let empty_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    empty_box.set_halign(gtk::Align::Center);
    empty_box.set_valign(gtk::Align::Center);
    empty_box.append(&empty_icon);
    empty_box.append(&empty_title);
    empty_box.append(&empty_sub);
    empty_box.append(&pick_btn);

    // ---------- 扫描中页面 ----------
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_spinning(true);

    let loading_title = gtk::Label::new(Some(lang.t("正在扫描音乐…", "Scanning music…")));
    loading_title.add_css_class("empty-title");

    let loading_sub = gtk::Label::new(None);
    loading_sub.add_css_class("empty-sub");
    loading_sub.set_ellipsize(pango::EllipsizeMode::Middle);
    loading_sub.set_max_width_chars(60);

    let loading_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
    loading_box.set_halign(gtk::Align::Center);
    loading_box.set_valign(gtk::Align::Center);
    loading_box.append(&spinner);
    loading_box.append(&loading_title);
    loading_box.append(&loading_sub);

    // ---------- 曲目列表 ----------
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();

    let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let filter = gtk::CustomFilter::new({
        let query = query.clone();
        let state = library_state.clone();
        move |obj| {
            let Some(boxed) = obj.downcast_ref::<glib::BoxedAnyObject>() else {
                return false;
            };
            let track = boxed.borrow::<Rc<Track>>();
            if state.removed.borrow().contains(&track.path) {
                return false;
            }
            if state.favorites_only.get() && !state.favorites.borrow().contains(&track.path) {
                return false;
            }
            let query = query.borrow();
            query.is_empty() || track.matches(&query)
        }
    });

    {
        let state = library_state.clone();
        let filter = filter.clone();
        favorites_btn.connect_toggled(move |btn| {
            state.favorites_only.set(btn.is_active());
            filter.changed(gtk::FilterChange::Different);
        });
    }

    let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));

    let selection = gtk::SingleSelection::new(Some(filter_model.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);

    let row_map: RowMap = Rc::new(RefCell::new(Vec::new()));
    let now_playing_path: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    let row_cells: RowCellMap = Rc::new(RefCell::new(HashMap::new()));

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup({
        let state = library_state.clone();
        let filter = filter.clone();
        let row_cells = row_cells.clone();
        move |_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = build_row(state.lang.get());
            let cell: RowCell = Rc::new(RefCell::new(None));
            row_cells
                .borrow_mut()
                .insert(item.as_ptr() as usize, cell.clone());

            if let Some((favorite, delete)) = row_buttons(&row) {
                let state_favorite = state.clone();
                let filter_favorite = filter.clone();
                let cell_favorite = cell.clone();
                favorite.connect_clicked(move |btn| {
                    let Some(track) = cell_favorite.borrow().clone() else {
                        return;
                    };
                    let is_favorite = {
                        let mut favorites = state_favorite.favorites.borrow_mut();
                        if favorites.contains(&track.path) {
                            favorites.remove(&track.path);
                            false
                        } else {
                            favorites.insert(track.path.clone());
                            true
                        }
                    };
                    update_favorite_button(btn, is_favorite, state_favorite.lang.get());
                    filter_favorite.changed(gtk::FilterChange::Different);
                });

                let state_delete = state.clone();
                let filter_delete = filter.clone();
                let cell_delete = cell.clone();
                delete.connect_clicked(move |btn| {
                    let Some(track) = cell_delete.borrow().clone() else {
                        return;
                    };
                    let parent = btn
                        .root()
                        .and_then(|root| root.downcast::<gtk::Window>().ok());
                    delete_track(track, parent, state_delete.clone(), filter_delete.clone());
                });
            }

            item.set_child(Some(&row));
        }
    });
    factory.connect_bind({
        let row_map = row_map.clone();
        let now_playing = now_playing_path.clone();
        let state = library_state.clone();
        let row_cells = row_cells.clone();
        move |_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row) = item.child().and_then(|c| c.downcast::<gtk::Box>().ok()) else {
                return;
            };
            let Some(boxed) = item
                .item()
                .and_then(|o| o.downcast::<glib::BoxedAnyObject>().ok())
            else {
                return;
            };
            let track = boxed.borrow::<Rc<Track>>().clone();
            let Some((title, subtitle, duration)) = row_labels(&row) else {
                return;
            };

            title.set_label(&track.title);
            subtitle.set_label(&track.subtitle());
            duration.set_label(&format_time(track.duration));

            if let Some(cell) = row_cells.borrow().get(&(item.as_ptr() as usize)).cloned() {
                *cell.borrow_mut() = Some(track.clone());
            }

            if let Some((favorite, _)) = row_buttons(&row) {
                update_favorite_button(
                    &favorite,
                    state.favorites.borrow().contains(&track.path),
                    state.lang.get(),
                );
            }

            row.remove_css_class("playing");
            if now_playing.borrow().as_deref() == Some(track.path.as_path()) {
                row.add_css_class("playing");
            }

            let mut rows = row_map.borrow_mut();
            rows.retain(|(_, widget)| widget != &row);
            rows.push((track.path.clone(), row));
        }
    });
    factory.connect_teardown({
        let row_cells = row_cells.clone();
        move |_, obj| {
            if let Some(item) = obj.downcast_ref::<gtk::ListItem>() {
                row_cells.borrow_mut().remove(&(item.as_ptr() as usize));
            }
        }
    });
    factory.connect_unbind({
        let row_map = row_map.clone();
        move |_, obj| {
            let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row) = item.child().and_then(|c| c.downcast::<gtk::Box>().ok()) else {
                return;
            };
            row_map.borrow_mut().retain(|(_, widget)| widget != &row);
        }
    });

    let list_view = gtk::ListView::new(Some(selection), Some(factory));
    list_view.set_show_separators(false);
    list_view.set_single_click_activate(false);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&list_view)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let list_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list_page.set_margin_top(6);
    list_page.append(&scrolled);

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();
    stack.set_transition_duration(220);
    stack.add_named(&empty_box, Some(STACK_EMPTY));
    stack.add_named(&loading_box, Some(STACK_LOADING));
    stack.add_named(&list_page, Some(STACK_LIST));
    stack.set_visible_child_name(STACK_EMPTY);

    // ---------- 播放控制栏 ----------
    let now_title = gtk::Label::new(Some(lang.t("未在播放", "Nothing playing")));
    now_title.set_xalign(0.0);
    now_title.set_ellipsize(pango::EllipsizeMode::End);
    now_title.set_max_width_chars(28);
    now_title.add_css_class("now-title");

    let now_artist = gtk::Label::new(Some(lang.t(
        "选择一首歌曲开始播放",
        "Pick a song to start playing",
    )));
    now_artist.set_xalign(0.0);
    now_artist.set_ellipsize(pango::EllipsizeMode::End);
    now_artist.set_max_width_chars(28);
    now_artist.add_css_class("now-artist");

    let info_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info_box.set_width_request(230);
    info_box.set_halign(gtk::Align::Start);
    info_box.set_valign(gtk::Align::Center);
    info_box.append(&now_title);
    info_box.append(&now_artist);

    let shuffle_icon = gtk::Image::from_icon_name("media-playlist-shuffle-symbolic");
    shuffle_icon.set_pixel_size(18);
    let shuffle_btn = gtk::Button::builder()
        .child(&shuffle_icon)
        .tooltip_text(lang.t("随机播放：关", "Shuffle: off"))
        .css_classes(["flat", "control-button"])
        .build();

    let prev_icon = gtk::Image::from_icon_name("media-skip-backward-symbolic");
    prev_icon.set_pixel_size(18);
    let prev_btn = gtk::Button::builder()
        .child(&prev_icon)
        .tooltip_text(lang.t("上一首", "Previous"))
        .css_classes(["flat", "control-button"])
        .build();

    let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
    play_icon.set_pixel_size(20);
    let play_btn = gtk::Button::builder()
        .child(&play_icon)
        .tooltip_text(lang.t("播放 / 暂停", "Play / Pause"))
        .css_classes(["suggested-action", "play-button"])
        .build();

    let next_icon = gtk::Image::from_icon_name("media-skip-forward-symbolic");
    next_icon.set_pixel_size(18);
    let next_btn = gtk::Button::builder()
        .child(&next_icon)
        .tooltip_text(lang.t("下一首", "Next"))
        .css_classes(["flat", "control-button"])
        .build();

    let repeat_icon = gtk::Image::from_icon_name("media-playlist-repeat-symbolic");
    repeat_icon.set_pixel_size(18);
    let repeat_btn = gtk::Button::builder()
        .child(&repeat_icon)
        .tooltip_text(lang.t("循环播放：关", "Repeat: off"))
        .css_classes(["flat", "control-button"])
        .build();

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    buttons.set_halign(gtk::Align::Center);
    buttons.append(&shuffle_btn);
    buttons.append(&prev_btn);
    buttons.append(&play_btn);
    buttons.append(&next_btn);
    buttons.append(&repeat_btn);

    let speed_label = gtk::Label::new(Some("1.0x"));
    speed_label.add_css_class("speed-label");
    let speed_btn = gtk::Button::builder()
        .child(&speed_label)
        .tooltip_text(lang.t("播放速度：1.0x（点击切换）", "Speed: 1.0x (click to change)"))
        .css_classes(["flat", "speed-button"])
        .build();

    let lyrics_label = gtk::Label::new(Some(lang.t("词", "LRC")));
    lyrics_label.add_css_class("lyrics-button-label");
    let lyrics_btn = gtk::Button::builder()
        .child(&lyrics_label)
        .tooltip_text(lang.t("显示 / 隐藏滚动歌词", "Show / hide scrolling lyrics"))
        .css_classes(["flat", "lyrics-button"])
        .build();

    let elapsed = gtk::Label::new(Some("0:00"));
    elapsed.add_css_class("time-label");
    elapsed.set_width_chars(4);
    elapsed.set_xalign(1.0);

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    seek.add_css_class("seek-scale");

    let total = gtk::Label::new(Some("0:00"));
    total.add_css_class("time-label");
    total.set_width_chars(4);
    total.set_xalign(0.0);

    let seek_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    seek_row.append(&elapsed);
    seek_row.append(&seek);
    seek_row.append(&total);

    let lyric_label = gtk::Label::new(None);
    lyric_label.add_css_class("lyric-line");
    lyric_label.set_ellipsize(pango::EllipsizeMode::End);
    lyric_label.set_single_line_mode(true);
    lyric_label.set_xalign(0.5);
    lyric_label.set_hexpand(true);
    lyric_label.set_max_width_chars(90);

    let lyric_revealer = gtk::Revealer::new();
    lyric_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    lyric_revealer.set_transition_duration(220);
    lyric_revealer.set_child(Some(&lyric_label));
    lyric_revealer.set_reveal_child(false);

    let mut player = Player::new();
    if let Some(volume) = saved.volume {
        player.set_volume(volume / 100.0);
    }

    let volume_icon = gtk::Image::from_icon_name("audio-volume-high-symbolic");
    volume_icon.add_css_class("dim-icon");
    let volume_btn = gtk::Button::builder()
        .child(&volume_icon)
        .tooltip_text(lang.t("静音", "Mute"))
        .css_classes(["flat", "volume-button"])
        .build();

    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 100.0, 1.0);
    volume.set_draw_value(false);
    volume.set_value((player.volume() as f64 * 100.0).round());
    volume.set_size_request(100, -1);
    volume.set_tooltip_text(Some(lang.t("音量", "Volume")));
    volume.add_css_class("volume-scale");

    let volume_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    volume_box.set_halign(gtk::Align::End);
    volume_box.set_valign(gtk::Align::Center);
    volume_box.append(&lyrics_btn);
    volume_box.append(&speed_btn);
    volume_box.append(&volume_btn);
    volume_box.append(&volume);

    let top_row = gtk::CenterBox::new();
    top_row.set_start_widget(Some(&info_box));
    top_row.set_center_widget(Some(&buttons));
    top_row.set_end_widget(Some(&volume_box));

    let player_bar = gtk::Box::new(gtk::Orientation::Vertical, 4);
    player_bar.add_css_class("player-bar");
    player_bar.append(&top_row);
    player_bar.append(&lyric_revealer);
    player_bar.append(&seek_row);

    // ---------- 滚动歌词面板 ----------
    let lyrics_list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    lyrics_list.add_css_class("lyrics-list");
    let lyric_pad_top = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let lyric_pad_bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let placeholder = gtk::Label::new(Some(lang.t("暂无歌词", "No lyrics")));
    placeholder.add_css_class("lyric-placeholder");
    lyrics_list.append(&lyric_pad_top);
    lyrics_list.append(&placeholder);
    lyrics_list.append(&lyric_pad_bottom);

    let lyrics_scroll = gtk::ScrolledWindow::new();
    lyrics_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    lyrics_scroll.set_child(Some(&lyrics_list));
    lyrics_scroll.add_css_class("lyrics-scroll");
    lyrics_scroll.set_min_content_height(180);
    lyrics_scroll.set_max_content_height(180);

    let lyrics_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    lyrics_panel.add_css_class("lyrics-panel");
    lyrics_panel.append(&lyrics_scroll);

    let lyrics_panel_revealer = gtk::Revealer::new();
    lyrics_panel_revealer.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    lyrics_panel_revealer.set_transition_duration(220);
    lyrics_panel_revealer.set_child(Some(&lyrics_panel));
    lyrics_panel_revealer.set_reveal_child(false);

    // ---------- 主窗口 ----------
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.set_margin_start(12);
    root.set_margin_end(12);
    root.set_margin_bottom(12);
    root.append(&stack);
    root.append(&lyrics_panel_revealer);
    root.append(&player_bar);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("RustMusic")
        .default_width(1000)
        .default_height(660)
        .build();
    // 禁止窗口放大/最大化
    window.set_resizable(false);
    window.set_icon_name(Some("io.github.rustmusic.RustMusic"));
    window.set_titlebar(Some(&header));
    window.set_child(Some(&root));

    let ui = Rc::new(Ui {
        window,
        library_state: library_state.clone(),
        player: RefCell::new(player),
        store,
        filter,
        filter_model,
        list_view,
        search,
        import_btn,
        pick_btn,
        prev_btn,
        play_btn,
        next_btn,
        shuffle_btn,
        repeat_btn,
        volume_btn,
        speed_btn,
        speed_label,
        settings_btn,
        favorites_btn,
        favorites_label,
        import_label,
        query,
        stack,
        empty_title,
        empty_sub,
        loading_title,
        loading_sub,
        now_title,
        now_artist,
        play_icon,
        repeat_icon,
        volume_icon,
        lyric_label,
        lyric_revealer,
        lyrics_btn,
        lyrics_btn_label: lyrics_label,
        lyrics_panel_revealer,
        lyrics_scroll,
        lyrics_list,
        lyric_widgets: RefCell::new(Vec::new()),
        lyric_pad_top,
        lyric_pad_bottom,
        lyrics_open: Cell::new(false),
        settings_window: RefCell::new(None),
        help_window: RefCell::new(None),
        lyric_scroll_gen: Rc::new(Cell::new(0)),
        lyrics: RefCell::new(Vec::new()),
        lyric_index: Cell::new(usize::MAX),
        speed_index: Cell::new(2),
        seek,
        elapsed,
        total,
        volume,
        queue: RefCell::new(Vec::new()),
        order: RefCell::new(Vec::new()),
        order_pos: Cell::new(0),
        shuffle: Cell::new(false),
        repeat: Cell::new(RepeatMode::Off),
        saved_volume: Cell::new(80.0),
        last_folder: RefCell::new(None),
        now_playing_path,
        row_map,
        current_duration: Cell::new(Duration::ZERO),
        pending_seek: Cell::new(None),
        last_seek_change: Cell::new(None),
        updating_seek: Cell::new(false),
        empty_kind: Cell::new(EmptyKind::Welcome),
        empty_folder: RefCell::new(String::new()),
        lyrics_panel_height: Cell::new(0),
        lyrics_recenter_pending: Cell::new(false),
    });

    ui.connect_events();
    ui.spawn_ticker();
    ui.apply_language();

    let ui_close = ui.clone();
    let app_close = app.clone();
    ui.window.connect_close_request(move |_| {
        ui_close.save_state();
        app_close.quit();
        glib::Propagation::Stop
    });

    {
        let quit = gio::SimpleAction::new("quit", None);
        let ui_quit = ui.clone();
        let app_quit = app.clone();
        quit.connect_activate(move |_, _| {
            ui_quit.save_state();
            app_quit.quit();
        });
        app.add_action(&quit);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
    }

    {
        let help = gio::SimpleAction::new("help", None);
        let ui_help = ui.clone();
        help.connect_activate(move |_, _| show_help_dialog(&ui_help));
        app.add_action(&help);
        app.set_accels_for_action("app.help", &["F1"]);

        let settings = gio::SimpleAction::new("settings", None);
        let ui_settings = ui.clone();
        settings.connect_activate(move |_, _| show_settings_dialog(&ui_settings));
        app.add_action(&settings);
        app.set_accels_for_action("app.settings", &["<Control>comma"]);

        let ui_btn = ui.clone();
        ui.settings_btn.connect_clicked(move |_| {
            show_settings_dialog(&ui_btn);
        });
    }

    if let Some(folder) = saved.folder {
        ui.start_scan(folder);
    }

    ui.window.present();

    spawn_tray(ui.clone(), app.clone());
}

fn show_help_dialog(ui: &Rc<Ui>) {
    if let Some(window) = ui.help_window.borrow().as_ref() {
        window.present();
        return;
    }

    let lang = ui.library_state.lang.get();
    let dialog = gtk::Window::builder()
        .transient_for(&ui.window)
        .modal(true)
        .title(lang.t("使用帮助", "Help"))
        .default_width(580)
        .build();
    dialog.add_css_class("help-dialog");
    dialog.add_css_class("dialog-window");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.add_css_class("help-content");

    let title = gtk::Label::new(Some(lang.t("RustMusic 使用帮助", "RustMusic Help")));
    title.add_css_class("help-title");
    title.set_xalign(0.0);

    let body = gtk::Label::new(None);
    body.set_markup(lang.t(
        "<b>导入音乐</b>\n点击右上角「导入文件夹」选择音乐目录。\n\n\
         <b>播放</b>\n双击歌曲播放；空格播放 / 暂停；← / → 快退 / 快进 5 秒；↑ / ↓ 调节音量；Ctrl+F 搜索。\n\n\
         <b>歌词</b>\n点击播放栏「词」按钮打开滚动歌词，支持内嵌歌词和同名 .lrc。\n\n\
         <b>收藏与删除</b>\n歌曲右侧星标收藏；顶栏「收藏夹」只看收藏；垃圾桶可以移除或删除文件。\n\n\
         <b>后台播放</b>\n最小化窗口后音乐继续播放；关闭窗口或按 Ctrl+Q 会退出程序。\n\n\
         <b>倍速</b>\n点击播放栏的速度按钮循环切换。",
        "<b>Import</b>\nUse “Import Folder” in the top-right to choose a music folder.\n\n\
         <b>Playback</b>\nDouble-click a song to play; Space to play/pause; ←/→ to seek 5 seconds; ↑/↓ to change volume; Ctrl+F to search.\n\n\
         <b>Lyrics</b>\nClick “LRC” in the player bar to show scrolling lyrics. Embedded lyrics and sidecar .lrc files are supported.\n\n\
         <b>Favorites & delete</b>\nStar a song to favorite it; “Favorites” filters the list; the trash button removes it from the list or deletes the file.\n\n\
         <b>Background</b>\nMinimize the window to keep playing; close the window or press Ctrl+Q to quit.\n\n\
         <b>Speed</b>\nClick the speed button to cycle through playback speeds.",
    ));
    body.set_wrap(true);
    body.set_xalign(0.0);
    body.add_css_class("help-body");

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions.set_halign(gtk::Align::End);
    let ok = gtk::Button::with_label(lang.t("知道了", "Got it"));
    ok.add_css_class("suggested-action");
    ok.add_css_class("help-action");
    actions.append(&ok);

    content.append(&title);
    content.append(&body);
    content.append(&actions);
    dialog.set_child(Some(&content));
    *ui.help_window.borrow_mut() = Some(dialog.clone());

    let ui_close = ui.clone();
    dialog.connect_close_request(move |_| {
        *ui_close.help_window.borrow_mut() = None;
        glib::Propagation::Proceed
    });

    let ui_destroy = ui.clone();
    dialog.connect_destroy(move |_| {
        *ui_destroy.help_window.borrow_mut() = None;
    });

    let dialog_ok = dialog.clone();
    ok.connect_clicked(move |_| {
        dialog_ok.close();
    });

    dialog.present();
}

fn show_settings_dialog(ui: &Rc<Ui>) {
    if let Some(window) = ui.settings_window.borrow().as_ref() {
        window.present();
        return;
    }

    let lang = ui.library_state.lang.get();
    let dialog = gtk::Window::builder()
        .transient_for(&ui.window)
        .modal(true)
        .title(lang.t("设置", "Settings"))
        .default_width(460)
        .build();
    dialog.add_css_class("settings-dialog");
    dialog.add_css_class("dialog-window");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.add_css_class("settings-content");

    let lang_section = gtk::Label::new(Some(lang.t("语言", "Language")));
    lang_section.add_css_class("settings-section");
    lang_section.set_xalign(0.0);

    let lang_label = gtk::Label::new(Some(lang.t("界面语言", "Interface language")));
    let lang_dropdown = gtk::DropDown::from_strings(&["中文", "English"]);
    lang_dropdown.set_selected(if lang == Lang::Zh { 0 } else { 1 });
    lang_dropdown.set_width_request(140);
    lang_dropdown.set_halign(gtk::Align::End);

    let help_label = gtk::Label::new(Some(lang.t("使用帮助", "Help")));
    let help_open_btn = gtk::Button::with_label(lang.t("打开帮助", "Open Help"));

    let about = gtk::Label::new(Some(lang.t(
        "RustMusic 0.1.1",
        "RustMusic 0.1.1",
    )));
    about.add_css_class("settings-about");
    about.set_xalign(0.0);

    let done = gtk::Button::with_label(lang.t("完成", "Done"));
    done.add_css_class("suggested-action");
    done.add_css_class("help-action");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions.set_halign(gtk::Align::End);
    actions.append(&done);

    content.append(&lang_section);
    content.append(&settings_row(&lang_label, &lang_dropdown));
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&settings_row(&help_label, &help_open_btn));
    content.append(&about);
    content.append(&actions);
    dialog.set_child(Some(&content));
    *ui.settings_window.borrow_mut() = Some(dialog.clone());

    let refresh = {
        let dialog = dialog.clone();
        let lang_section = lang_section.clone();
        let lang_label = lang_label.clone();
        let help_label = help_label.clone();
        let help_open_btn = help_open_btn.clone();
        let about = about.clone();
        let done = done.clone();
        let ui = ui.clone();
        Rc::new(move |lang: Lang| {
            dialog.set_title(Some(lang.t("设置", "Settings")));
            lang_section.set_label(lang.t("语言", "Language"));
            lang_label.set_label(lang.t("界面语言", "Interface language"));
            help_label.set_label(lang.t("使用帮助", "Help"));
            help_open_btn.set_label(lang.t("打开帮助", "Open Help"));
            about.set_label(lang.t(
                "RustMusic 0.1.1 · 纯 Rust + GTK4",
                "RustMusic 0.1.1 · Pure Rust + GTK4",
            ));
            done.set_label(lang.t("完成", "Done"));
            ui.apply_language();
        })
    };

    {
        let ui_lang = ui.clone();
        let refresh = refresh.clone();
        lang_dropdown.connect_selected_notify(move |dropdown| {
            let target = if dropdown.selected() == 0 {
                Lang::Zh
            } else {
                Lang::En
            };
            if target == ui_lang.library_state.lang.get() {
                return;
            }
            ui_lang.library_state.lang.set(target);
            ui_lang.save_state();
            refresh(target);
        });
    }
    {
        let ui_help = ui.clone();
        help_open_btn.connect_clicked(move |_| {
            show_help_dialog(&ui_help);
        });
    }

    let settings_done = dialog.clone();
    done.connect_clicked(move |_| {
        settings_done.close();
    });

    let ui_close = ui.clone();
    dialog.connect_close_request(move |_| {
        *ui_close.settings_window.borrow_mut() = None;
        glib::Propagation::Proceed
    });

    let ui_destroy = ui.clone();
    dialog.connect_destroy(move |_| {
        *ui_destroy.settings_window.borrow_mut() = None;
    });

    dialog.present();
}

fn settings_row(label: &gtk::Label, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    row.add_css_class("settings-row");
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.add_css_class("settings-label");
    row.append(label);
    control.set_valign(gtk::Align::Center);
    row.append(control);
    row
}

fn logo_widget(pixel_size: i32) -> gtk::Widget {
    if let Some(pixbuf) = scaled_logo(pixel_size) {
        let texture = gdk::Texture::for_pixbuf(&pixbuf);
        let image = gtk::Image::from_paintable(Some(&texture));
        image.set_pixel_size(pixel_size);
        image.set_halign(gtk::Align::Center);
        image.set_valign(gtk::Align::Center);
        return image.upcast();
    }

    let image = gtk::Image::from_icon_name("folder-music-symbolic");
    image.set_pixel_size(pixel_size);
    image.upcast()
}

fn scaled_logo(pixel_size: i32) -> Option<gdk_pixbuf::Pixbuf> {
    use gdk_pixbuf::prelude::*;

    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(include_bytes!("logo.svg")).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    pixbuf.scale_simple(
        pixel_size,
        pixel_size,
        gdk_pixbuf::InterpType::Bilinear,
    )
}

fn build_row(lang: Lang) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("track-row");

    let info = gtk::Box::new(gtk::Orientation::Vertical, 1);
    info.set_hexpand(true);
    info.set_valign(gtk::Align::Center);

    let title = gtk::Label::new(None);
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::End);
    title.set_single_line_mode(true);
    title.add_css_class("track-title");

    let subtitle = gtk::Label::new(None);
    subtitle.set_xalign(0.0);
    subtitle.set_ellipsize(pango::EllipsizeMode::End);
    subtitle.set_single_line_mode(true);
    subtitle.add_css_class("track-subtitle");

    info.append(&title);
    info.append(&subtitle);

    let duration = gtk::Label::new(None);
    duration.set_valign(gtk::Align::Center);
    duration.set_width_chars(5);
    duration.set_xalign(1.0);
    duration.add_css_class("track-duration");

    let favorite_icon = gtk::Image::from_icon_name("non-starred-symbolic");
    favorite_icon.set_pixel_size(16);
    let favorite_btn = gtk::Button::builder()
        .child(&favorite_icon)
        .tooltip_text(lang.t("收藏", "Add to favorites"))
        .css_classes(["flat", "row-action", "favorite-button"])
        .build();

    let delete_icon = gtk::Image::from_icon_name("user-trash-symbolic");
    delete_icon.set_pixel_size(16);
    let delete_btn = gtk::Button::builder()
        .child(&delete_icon)
        .tooltip_text(lang.t("删除", "Delete"))
        .css_classes(["flat", "row-action", "delete-button"])
        .build();

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    actions.set_valign(gtk::Align::Center);
    actions.append(&favorite_btn);
    actions.append(&delete_btn);

    row.append(&info);
    row.append(&duration);
    row.append(&actions);
    row
}

fn row_labels(row: &gtk::Box) -> Option<(gtk::Label, gtk::Label, gtk::Label)> {
    let info = row.first_child()?.downcast::<gtk::Box>().ok()?;
    let title = info.first_child()?.downcast::<gtk::Label>().ok()?;
    let subtitle = title.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let duration = info.next_sibling()?.downcast::<gtk::Label>().ok()?;
    Some((title, subtitle, duration))
}

fn row_buttons(row: &gtk::Box) -> Option<(gtk::Button, gtk::Button)> {
    let info = row.first_child()?.downcast::<gtk::Box>().ok()?;
    let duration = info.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let actions = duration.next_sibling()?.downcast::<gtk::Box>().ok()?;
    let favorite = actions.first_child()?.downcast::<gtk::Button>().ok()?;
    let delete = favorite.next_sibling()?.downcast::<gtk::Button>().ok()?;
    Some((favorite, delete))
}

fn update_favorite_button(button: &gtk::Button, favorite: bool, lang: Lang) {
    if let Some(image) = button
        .child()
        .and_then(|child| child.downcast::<gtk::Image>().ok())
    {
        image.set_icon_name(Some(if favorite {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        }));
    }
    if favorite {
        button.add_css_class("favorite-active");
    } else {
        button.remove_css_class("favorite-active");
    }
    button.set_tooltip_text(Some(if favorite {
        lang.t("取消收藏", "Remove from favorites")
    } else {
        lang.t("收藏", "Add to favorites")
    }));
}

fn delete_track(
    track: Rc<Track>,
    parent: Option<gtk::Window>,
    state: Rc<LibraryState>,
    filter: gtk::CustomFilter,
) {
    let lang = state.lang.get();
    let dialog = gtk::Window::builder()
        .title(lang.t("删除音乐", "Delete music"))
        .modal(true)
        .resizable(false)
        .default_width(460)
        .build();
    if let Some(parent) = parent.as_ref() {
        dialog.set_transient_for(Some(parent));
    }
    dialog.add_css_class("dialog-window");
    dialog.add_css_class("delete-dialog");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.add_css_class("delete-content");

    let title = gtk::Label::new(Some(lang.t("删除音乐", "Delete music")));
    title.add_css_class("dialog-title");
    title.set_xalign(0.0);

    let track_title = gtk::Label::new(Some(&track.title));
    track_title.add_css_class("delete-track-title");
    track_title.set_xalign(0.0);
    track_title.set_ellipsize(pango::EllipsizeMode::End);

    let track_sub = gtk::Label::new(Some(&track.subtitle()));
    track_sub.add_css_class("delete-track-sub");
    track_sub.set_xalign(0.0);
    track_sub.set_ellipsize(pango::EllipsizeMode::End);

    let body = gtk::Label::new(Some(lang.t(
        "从列表移除不会删除文件；删除文件会永久删除磁盘上的音乐。",
        "Removing from the list keeps the file; deleting the file removes it from disk.",
    )));
    body.add_css_class("delete-body");
    body.set_wrap(true);
    body.set_xalign(0.0);

    let error = gtk::Label::new(None);
    error.add_css_class("delete-error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(10);
    let cancel = gtk::Button::with_label(lang.t("取消", "Cancel"));
    cancel.add_css_class("flat");
    let remove = gtk::Button::with_label(lang.t("从列表移除", "Remove from list"));
    remove.add_css_class("delete-remove");
    let delete = gtk::Button::with_label(lang.t("删除文件", "Delete file"));
    delete.add_css_class("destructive-action");
    delete.add_css_class("delete-confirm");
    actions.append(&cancel);
    actions.append(&remove);
    actions.append(&delete);

    content.append(&title);
    content.append(&track_title);
    content.append(&track_sub);
    content.append(&body);
    content.append(&error);
    content.append(&actions);
    dialog.set_child(Some(&content));

    let cancel_dialog = dialog.clone();
    cancel.connect_clicked(move |_| {
        cancel_dialog.close();
    });

    {
        let state = state.clone();
        let filter = filter.clone();
        let dialog = dialog.clone();
        let path = track.path.clone();
        remove.connect_clicked(move |_| {
            state.removed.borrow_mut().insert(path.clone());
            state.favorites.borrow_mut().remove(&path);
            filter.changed(gtk::FilterChange::Different);
            dialog.close();
        });
    }
    {
        let state = state.clone();
        let filter = filter.clone();
        let dialog = dialog.clone();
        let error = error.clone();
        let path = track.path.clone();
        delete.connect_clicked(move |_| {
            if let Err(err) = fs::remove_file(&path) {
                error.set_label(&format!(
                    "{}\n{}",
                    lang.t("删除文件失败：", "Failed to delete file:"),
                    err
                ));
                error.set_visible(true);
                return;
            }
            state.removed.borrow_mut().insert(path.clone());
            state.favorites.borrow_mut().remove(&path);
            filter.changed(gtk::FilterChange::Different);
            dialog.close();
        });
    }

    dialog.present();
}

struct Ui {
    window: gtk::ApplicationWindow,
    library_state: Rc<LibraryState>,
    player: RefCell<Player>,
    store: gio::ListStore,
    filter: gtk::CustomFilter,
    filter_model: gtk::FilterListModel,
    list_view: gtk::ListView,
    search: gtk::SearchEntry,
    import_btn: gtk::Button,
    pick_btn: gtk::Button,
    prev_btn: gtk::Button,
    play_btn: gtk::Button,
    next_btn: gtk::Button,
    shuffle_btn: gtk::Button,
    repeat_btn: gtk::Button,
    volume_btn: gtk::Button,
    speed_btn: gtk::Button,
    speed_label: gtk::Label,
    settings_btn: gtk::Button,
    favorites_btn: gtk::ToggleButton,
    favorites_label: gtk::Label,
    import_label: gtk::Label,
    query: Rc<RefCell<String>>,
    stack: gtk::Stack,
    empty_title: gtk::Label,
    empty_sub: gtk::Label,
    loading_title: gtk::Label,
    loading_sub: gtk::Label,
    now_title: gtk::Label,
    now_artist: gtk::Label,
    play_icon: gtk::Image,
    repeat_icon: gtk::Image,
    volume_icon: gtk::Image,
    lyric_label: gtk::Label,
    lyric_revealer: gtk::Revealer,
    lyrics_btn: gtk::Button,
    lyrics_btn_label: gtk::Label,
    lyrics_panel_revealer: gtk::Revealer,
    lyrics_scroll: gtk::ScrolledWindow,
    lyrics_list: gtk::Box,
    lyric_widgets: RefCell<Vec<gtk::Label>>,
    lyric_pad_top: gtk::Box,
    lyric_pad_bottom: gtk::Box,
    lyrics_open: Cell<bool>,
    settings_window: RefCell<Option<gtk::Window>>,
    help_window: RefCell<Option<gtk::Window>>,
    lyric_scroll_gen: Rc<Cell<u64>>,
    lyrics: RefCell<Vec<LyricLine>>,
    lyric_index: Cell<usize>,
    speed_index: Cell<usize>,
    seek: gtk::Scale,
    elapsed: gtk::Label,
    total: gtk::Label,
    volume: gtk::Scale,
    queue: RefCell<Vec<Rc<Track>>>,
    order: RefCell<Vec<usize>>,
    order_pos: Cell<usize>,
    shuffle: Cell<bool>,
    repeat: Cell<RepeatMode>,
    saved_volume: Cell<f64>,
    last_folder: RefCell<Option<PathBuf>>,
    now_playing_path: Rc<RefCell<Option<PathBuf>>>,
    row_map: RowMap,
    current_duration: Cell<Duration>,
    pending_seek: Cell<Option<f64>>,
    last_seek_change: Cell<Option<Instant>>,
    updating_seek: Cell<bool>,
    empty_kind: Cell<EmptyKind>,
    empty_folder: RefCell<String>,
    lyrics_panel_height: Cell<i32>,
    lyrics_recenter_pending: Cell<bool>,
}

impl Ui {
    fn connect_events(self: &Rc<Self>) {
        self.window.connect_maximized_notify(|window| {
            if window.is_maximized() {
                window.add_css_class("maximized-window");
            } else {
                window.remove_css_class("maximized-window");
            }
        });

        let ui = self.clone();
        self.import_btn.connect_clicked(move |_| ui.choose_folder());

        let ui = self.clone();
        self.pick_btn.connect_clicked(move |_| ui.choose_folder());

        let ui = self.clone();
        self.search.connect_search_changed(move |entry| {
            *ui.query.borrow_mut() = entry.text().to_lowercase();
            ui.filter.changed(gtk::FilterChange::Different);
        });

        let ui = self.clone();
        self.play_btn.connect_clicked(move |_| ui.toggle_play());

        let ui = self.clone();
        self.prev_btn.connect_clicked(move |_| ui.play_prev());

        let ui = self.clone();
        self.next_btn.connect_clicked(move |_| ui.play_next());

        let ui = self.clone();
        self.shuffle_btn.connect_clicked(move |_| ui.toggle_shuffle());

        let ui = self.clone();
        self.repeat_btn.connect_clicked(move |_| ui.toggle_repeat());

        let ui = self.clone();
        self.volume_btn.connect_clicked(move |_| ui.toggle_mute());

        let ui = self.clone();
        self.speed_btn.connect_clicked(move |_| ui.cycle_speed());

        let ui = self.clone();
        self.lyrics_btn.connect_clicked(move |btn| {
            let open = !ui.lyrics_open.get();
            ui.lyrics_open.set(open);
            ui.lyrics_panel_revealer.set_reveal_child(open);
            if open {
                btn.add_css_class("mode-active");
                let ui = ui.clone();
                glib::timeout_add_local_once(Duration::from_millis(240), move || {
                    ui.update_lyric_pads();
                    ui.scroll_lyrics_to_active();
                });
            } else {
                btn.remove_css_class("mode-active");
            }
        });

        let ui = self.clone();
        self.seek.connect_value_changed(move |scale| {
            if ui.updating_seek.get() {
                return;
            }
            if ui.player.borrow().state() == PlaybackState::Stopped {
                ui.updating_seek.set(true);
                scale.set_value(0.0);
                ui.updating_seek.set(false);
                return;
            }
            ui.pending_seek.set(Some(scale.value()));
            ui.last_seek_change.set(Some(Instant::now()));
            ui.elapsed.set_label(&format_seconds(scale.value()));
        });

        let ui = self.clone();
        self.volume.connect_value_changed(move |scale| {
            ui.player
                .borrow_mut()
                .set_volume((scale.value() / 100.0) as f32);
            ui.update_volume_icon(scale.value());
        });

        let ui = self.clone();
        self.list_view.connect_activate(move |_, position| {
            ui.play_from_view(position);
        });

        let controller = gtk::EventControllerKey::new();
        let ui = self.clone();
        controller.connect_key_pressed(move |_, key, _, modifiers| {
            if modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && (key == gdk::Key::f || key == gdk::Key::F)
            {
                ui.search.grab_focus();
                return glib::Propagation::Stop;
            }

            if ui.is_editing() {
                if key == gdk::Key::Escape {
                    ui.search.set_text("");
                    ui.list_view.grab_focus();
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }

            match key {
                gdk::Key::space => {
                    ui.toggle_play();
                    glib::Propagation::Stop
                }
                gdk::Key::Left => {
                    ui.seek_relative(-5.0);
                    glib::Propagation::Stop
                }
                gdk::Key::Right => {
                    ui.seek_relative(5.0);
                    glib::Propagation::Stop
                }
                gdk::Key::Up => {
                    ui.adjust_volume(5.0);
                    glib::Propagation::Stop
                }
                gdk::Key::Down => {
                    ui.adjust_volume(-5.0);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.window.add_controller(controller);

        self.update_mode_buttons();
        self.update_volume_icon(self.volume.value());
    }

    fn spawn_ticker(self: &Rc<Self>) {
        let ui = self.clone();
        glib::timeout_add_local(TICK_INTERVAL, move || {
            ui.tick();
            ui.refresh_lyrics_layout();
            ui.apply_lyric_recenter();
            glib::ControlFlow::Continue
        });
    }

    fn lang(&self) -> Lang {
        self.library_state.lang.get()
    }

    fn apply_language(&self) {
        let lang = self.lang();
        self.search.set_placeholder_text(Some(lang.t(
            "搜索歌曲、艺术家或专辑",
            "Search songs, artists or albums",
        )));
        self.settings_btn
            .set_tooltip_text(Some(lang.t("设置 (Ctrl+,)", "Settings (Ctrl+,)")));
        self.favorites_label.set_label(lang.t("收藏夹", "Favorites"));
        self.favorites_btn
            .set_tooltip_text(Some(lang.t("只看收藏的歌曲", "Show favorites only")));
        self.import_label.set_label(lang.t("导入文件夹", "Import Folder"));
        self.import_btn.set_tooltip_text(Some(lang.t(
            "扫描一个文件夹并导入其中的音乐",
            "Scan a folder and import its music",
        )));
        self.pick_btn
            .set_label(lang.t("选择音乐文件夹", "Choose Music Folder"));
        self.prev_btn
            .set_tooltip_text(Some(lang.t("上一首", "Previous")));
        self.play_btn
            .set_tooltip_text(Some(lang.t("播放 / 暂停", "Play / Pause")));
        self.next_btn.set_tooltip_text(Some(lang.t("下一首", "Next")));
        self.lyrics_btn.set_tooltip_text(Some(lang.t(
            "显示 / 隐藏滚动歌词",
            "Show / hide scrolling lyrics",
        )));
        self.lyrics_btn_label.set_label(lang.t("词", "LRC"));
        self.volume
            .set_tooltip_text(Some(lang.t("音量", "Volume")));

        self.update_mode_buttons();
        self.update_volume_icon(self.volume.value());
        self.update_speed_tooltip();
        self.update_empty_text();
        self.update_loading_text();
        self.refresh_row_tooltips();
        if self.lyrics.borrow().is_empty() {
            self.rebuild_lyrics_panel();
        }
        if self.player.borrow().state() == PlaybackState::Stopped
            && self.now_playing_path.borrow().is_none()
        {
            self.now_title
                .set_label(lang.t("未在播放", "Nothing playing"));
            self.now_artist.set_label(lang.t(
                "选择一首歌曲开始播放",
                "Pick a song to start playing",
            ));
        }
    }

    fn update_empty_text(&self) {
        let lang = self.lang();
        match self.empty_kind.get() {
            EmptyKind::Welcome => {
                self.empty_title.set_label(lang.t("还没有音乐", "No music yet"));
                self.empty_sub.set_label(lang.t(
                    "点击右上角「导入文件夹」，或直接选择音乐目录",
                    "Use “Import Folder” in the top-right, or choose a music folder",
                ));
            }
            EmptyKind::NoMusic => {
                self.empty_title
                    .set_label(lang.t("没有找到可播放的音乐", "No playable music found"));
                self.empty_sub.set_label(&format!(
                    "{}{}",
                    lang.t("已扫描：", "Scanned: "),
                    self.empty_folder.borrow()
                ));
            }
            EmptyKind::ScanFailed => {
                self.empty_title.set_label(lang.t("扫描失败", "Scan failed"));
                self.empty_sub.set_label(lang.t(
                    "无法读取该文件夹，请重试",
                    "Could not read this folder. Please try again.",
                ));
            }
        }
    }

    fn update_loading_text(&self) {
        if self.stack.visible_child_name().as_deref() != Some(STACK_LOADING) {
            return;
        }
        self.loading_title
            .set_label(self.lang().t("正在扫描音乐…", "Scanning music…"));
        if let Some(folder) = self.last_folder.borrow().as_ref() {
            self.loading_sub.set_label(&folder.display().to_string());
        }
    }

    fn update_speed_tooltip(&self) {
        let lang = self.lang();
        let label = format_speed(SPEEDS[self.speed_index.get()]);
        self.speed_btn.set_tooltip_text(Some(&format!(
            "{}{}{}",
            lang.t("播放速度：", "Speed: "),
            label,
            lang.t("（点击切换）", " (click to change)")
        )));
    }

    fn refresh_row_tooltips(&self) {
        let lang = self.lang();
        let favorites = self.library_state.favorites.borrow();
        let rows = self.row_map.borrow();
        for (path, row) in rows.iter() {
            if let Some((favorite, delete)) = row_buttons(row) {
                update_favorite_button(&favorite, favorites.contains(path), lang);
                delete.set_tooltip_text(Some(lang.t("删除", "Delete")));
            }
        }
    }

    fn refresh_lyrics_layout(&self) {
        let height = self.window.height();
        if height <= 0 {
            return;
        }
        let target = ((f64::from(height) * 0.38) as i32).clamp(180, 460);
        if target != self.lyrics_panel_height.get() {
            self.lyrics_panel_height.set(target);
            self.lyrics_scroll.set_max_content_height(-1);
            self.lyrics_scroll.set_min_content_height(target);
            self.lyrics_scroll.set_max_content_height(target);
            if self.lyrics_open.get() {
                self.lyrics_recenter_pending.set(true);
            }
        }
    }

    fn apply_lyric_recenter(&self) {
        if !self.lyrics_recenter_pending.get() {
            return;
        }
        self.lyrics_recenter_pending.set(false);
        if self.lyrics_open.get() {
            self.update_lyric_pads();
            self.scroll_lyrics_to_active();
        }
    }

    fn update_lyric_pads(&self) {
        let viewport = self.lyrics_scroll.vadjustment().page_size().max(0.0) as i32;
        let pad = ((viewport - 48) / 2).max(0);
        self.lyric_pad_top.set_size_request(-1, pad);
        self.lyric_pad_bottom.set_size_request(-1, pad);
    }

    fn choose_folder(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title(self.lang().t("选择音乐文件夹", "Choose Music Folder"))
            .modal(true)
            .build();
        let ui = self.clone();
        dialog.select_folder(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        ui.start_scan(path);
                    }
                }
            },
        );
    }

    fn save_state(&self) {
        let folder = self.last_folder.borrow().clone();
        let volume = self.player.borrow().volume();
        let favorites = self.library_state.favorites.borrow().clone();
        let removed = self.library_state.removed.borrow().clone();
        save_config(
            folder.as_deref(),
            volume,
            &favorites,
            &removed,
            self.lang(),
        );
    }

    fn start_scan(self: &Rc<Self>, folder: PathBuf) {
        *self.last_folder.borrow_mut() = Some(folder.clone());
        self.save_state();

        self.player.borrow_mut().stop();
        self.reset_player_ui();
        self.loading_title
            .set_label(self.lang().t("正在扫描音乐…", "Scanning music…"));
        self.loading_sub.set_label(&folder.display().to_string());
        self.stack.set_visible_child_name(STACK_LOADING);
        self.import_btn.set_sensitive(false);

        let (tx, rx) = mpsc::channel();
        let scan_dir = folder.clone();
        std::thread::spawn(move || {
            let tracks = library::scan_folder(&scan_dir);
            let _ = tx.send((scan_dir, tracks));
        });

        let ui = self.clone();
        glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
            Ok((folder, tracks)) => {
                ui.import_btn.set_sensitive(true);
                ui.apply_scan(folder, tracks);
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                ui.import_btn.set_sensitive(true);
                ui.empty_kind.set(EmptyKind::ScanFailed);
                ui.update_empty_text();
                ui.stack.set_visible_child_name(STACK_EMPTY);
                glib::ControlFlow::Break
            }
        });
    }

    fn apply_scan(&self, folder: PathBuf, tracks: Vec<Track>) {
        self.reset_player_ui();
        self.store.remove_all();
        self.search.set_text("");

        let count = tracks.len();
        for track in tracks {
            self.store
                .append(&glib::BoxedAnyObject::new(Rc::new(track)));
        }

        let folder_name = folder
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| folder.display().to_string());

        if count == 0 {
            self.empty_folder
                .replace(folder.display().to_string());
            self.empty_kind.set(EmptyKind::NoMusic);
            self.update_empty_text();
            self.stack.set_visible_child_name(STACK_EMPTY);
        } else {
            self.empty_kind.set(EmptyKind::Welcome);
            self.update_empty_text();
            self.stack.set_visible_child_name(STACK_LIST);
            let window = self.window.clone();
            glib::idle_add_local_once(move || {
                gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
            });
        }

        self.window
            .set_title(Some(&format!("RustMusic — {folder_name}")));
    }

    fn reset_player_ui(&self) {
        self.queue.borrow_mut().clear();
        self.order.borrow_mut().clear();
        self.order_pos.set(0);
        self.current_duration.set(Duration::ZERO);
        *self.now_playing_path.borrow_mut() = None;
        self.row_map.borrow_mut().clear();
        self.pending_seek.set(None);
        self.lyrics.borrow_mut().clear();
        self.lyric_index.set(usize::MAX);
        self.lyric_label.set_text("");
        self.lyric_revealer.set_reveal_child(false);
        self.rebuild_lyrics_panel();

        self.now_title
            .set_label(self.lang().t("未在播放", "Nothing playing"));
        self.now_artist.set_label(self.lang().t(
            "选择一首歌曲开始播放",
            "Pick a song to start playing",
        ));
        self.elapsed.set_label("0:00");
        self.total.set_label("0:00");
        self.updating_seek.set(true);
        self.seek.set_range(0.0, 1.0);
        self.seek.set_value(0.0);
        self.updating_seek.set(false);
        self.set_playing_icon(false);
    }

    fn play_from_view(&self, position: u32) {
        let n_items = self.filter_model.n_items();
        if n_items == 0 {
            return;
        }

        let mut queue = Vec::with_capacity(n_items as usize);
        for index in 0..n_items {
            if let Some(obj) = self.filter_model.item(index) {
                if let Some(boxed) = obj.downcast_ref::<glib::BoxedAnyObject>() {
                    queue.push(boxed.borrow::<Rc<Track>>().clone());
                }
            }
        }
        if queue.is_empty() {
            return;
        }

        let pos = (position as usize).min(queue.len() - 1);
        self.queue.replace(queue);
        self.build_order(Some(pos));
        if let Some(qidx) = self.current_queue_index() {
            self.play_queue_index(qidx);
        }
    }

    fn current_queue_index(&self) -> Option<usize> {
        self.order.borrow().get(self.order_pos.get()).copied()
    }

    fn build_order(&self, anchor: Option<usize>) {
        let len = self.queue.borrow().len();
        let mut order: Vec<usize> = (0..len).collect();
        if self.shuffle.get() {
            fastrand::Rng::new().shuffle(&mut order);
            if let Some(anchor) = anchor {
                if let Some(index) = order.iter().position(|&i| i == anchor) {
                    order.swap(0, index);
                }
            }
            self.order_pos.set(0);
        } else {
            self.order_pos
                .set(anchor.unwrap_or(0).min(len.saturating_sub(1)));
        }
        self.order.replace(order);
    }

    fn play_queue_index(&self, pos: usize) {
        let Some(track) = self.queue.borrow().get(pos).cloned() else {
            return;
        };

        if let Err(err) = self.player.borrow_mut().play_file(&track.path) {
            let lang = self.lang();
            self.show_error(
                lang.t("无法播放该文件", "Unable to play this file"),
                &format!(
                    "{}\n\n{}\n\n{}",
                    err,
                    lang.t("文件：", "File: "),
                    format!(
                        "{}\n\n{}",
                        track.path.display(),
                        lang.t(
                            "应用会优先使用内置解码器，失败时自动调用 ffmpeg。请确认系统已安装 ffmpeg，并且文件没有损坏。",
                            "The built-in decoder is tried first, then ffmpeg. Make sure ffmpeg is installed and the file is not corrupted."
                        )
                    )
                ),
            );
            return;
        }

        self.current_duration.set(track.duration);

        let old = self.now_playing_path.borrow().clone();
        *self.now_playing_path.borrow_mut() = Some(track.path.clone());
        self.update_highlight(old.as_deref(), Some(track.path.as_path()));

        self.now_title.set_label(&track.title);
        self.now_artist.set_label(&track.subtitle());
        self.total.set_label(&format_time(track.duration));
        self.elapsed.set_label("0:00");
        self.pending_seek.set(None);
        self.updating_seek.set(true);
        self.seek
            .set_range(0.0, track.duration.as_secs_f64().max(0.1));
        self.seek.set_value(0.0);
        self.updating_seek.set(false);
        self.set_playing_icon(true);
        self.load_lyrics(&track.path, &track.title);
    }

    fn toggle_play(&self) {
        let state = self.player.borrow().state();
        match state {
            PlaybackState::Playing => {
                self.player.borrow_mut().pause();
                self.set_playing_icon(false);
            }
            PlaybackState::Paused => {
                self.player.borrow_mut().resume();
                self.set_playing_icon(true);
            }
            PlaybackState::Stopped => {
                if let Some(qidx) = self.current_queue_index() {
                    self.play_queue_index(qidx);
                } else if self.store.n_items() > 0 {
                    self.play_from_view(0);
                }
            }
        }
    }

    fn play_prev(&self) {
        let order_len = self.order.borrow().len();
        if order_len == 0 {
            return;
        }
        let pos = self.order_pos.get().min(order_len - 1);
        let elapsed = self.player.borrow().position();
        let target = if elapsed >= SEEK_RESTART_THRESHOLD {
            pos
        } else if pos > 0 {
            pos - 1
        } else if self.repeat.get() == RepeatMode::All {
            order_len - 1
        } else {
            pos
        };
        self.order_pos.set(target);
        if let Some(qidx) = self.current_queue_index() {
            self.play_queue_index(qidx);
        }
    }

    fn play_next(&self) {
        self.advance(false);
    }

    fn advance(&self, auto: bool) {
        let order_len = self.order.borrow().len();
        if order_len == 0 {
            return;
        }

        if auto && self.repeat.get() == RepeatMode::One {
            if let Some(qidx) = self.current_queue_index() {
                self.play_queue_index(qidx);
            }
            return;
        }

        let pos = self.order_pos.get();
        if pos + 1 < order_len {
            self.order_pos.set(pos + 1);
        } else if self.repeat.get() == RepeatMode::All {
            if self.shuffle.get() {
                self.build_order(None);
            } else {
                self.order_pos.set(0);
            }
        } else {
            self.player.borrow_mut().stop();
            self.set_playing_icon(false);
            self.reset_seek_position();
            return;
        }

        if let Some(qidx) = self.current_queue_index() {
            self.play_queue_index(qidx);
        }
    }

    fn toggle_shuffle(&self) {
        self.shuffle.set(!self.shuffle.get());
        if !self.queue.borrow().is_empty() {
            let current = self.current_queue_index();
            self.build_order(current);
        }
        self.update_mode_buttons();
    }

    fn toggle_repeat(&self) {
        let next = match self.repeat.get() {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.repeat.set(next);
        self.update_mode_buttons();
    }

    fn update_mode_buttons(&self) {
        let lang = self.lang();
        if self.shuffle.get() {
            self.shuffle_btn.add_css_class("mode-active");
        } else {
            self.shuffle_btn.remove_css_class("mode-active");
        }
        self.shuffle_btn.set_tooltip_text(Some(if self.shuffle.get() {
            lang.t("随机播放：开", "Shuffle: on")
        } else {
            lang.t("随机播放：关", "Shuffle: off")
        }));

        let (icon, tooltip, active) = match self.repeat.get() {
            RepeatMode::Off => (
                "media-playlist-repeat-symbolic",
                lang.t("循环播放：关", "Repeat: off"),
                false,
            ),
            RepeatMode::All => (
                "media-playlist-repeat-symbolic",
                lang.t("列表循环", "Repeat all"),
                true,
            ),
            RepeatMode::One => (
                "media-playlist-repeat-song-symbolic",
                lang.t("单曲循环", "Repeat one"),
                true,
            ),
        };
        self.repeat_icon.set_icon_name(Some(icon));
        self.repeat_btn.set_tooltip_text(Some(tooltip));
        if active {
            self.repeat_btn.add_css_class("mode-active");
        } else {
            self.repeat_btn.remove_css_class("mode-active");
        }
    }

    fn seek_relative(&self, delta_seconds: f64) {
        if self.player.borrow().state() == PlaybackState::Stopped {
            return;
        }
        let duration = self.current_duration.get();
        if duration.is_zero() {
            return;
        }
        let position = self.player.borrow().position();
        let target = (position.as_secs_f64() + delta_seconds)
            .clamp(0.0, duration.as_secs_f64());
        let target = Duration::from_secs_f64(target);
        let _ = self.player.borrow_mut().seek(target);
        self.elapsed.set_label(&format_time(target));
        self.updating_seek.set(true);
        self.seek.set_value(target.as_secs_f64());
        self.updating_seek.set(false);
        self.update_lyrics(target);
    }

    fn adjust_volume(&self, delta: f64) {
        let next = (self.volume.value() + delta).clamp(0.0, 100.0);
        self.volume.set_value(next);
    }

    fn update_volume_icon(&self, value: f64) {
        let icon = if value <= 0.5 {
            "audio-volume-muted-symbolic"
        } else if value < 34.0 {
            "audio-volume-low-symbolic"
        } else if value < 67.0 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        };
        self.volume_icon.set_icon_name(Some(icon));
        self.volume_btn.set_tooltip_text(Some(if value <= 0.5 {
            self.lang().t("取消静音", "Unmute")
        } else {
            self.lang().t("静音", "Mute")
        }));
    }

    fn toggle_mute(&self) {
        let current = self.volume.value();
        if current > 0.5 {
            self.saved_volume.set(current);
            self.volume.set_value(0.0);
        } else {
            self.volume.set_value(self.saved_volume.get().max(10.0));
        }
    }

    fn cycle_speed(&self) {
        let index = (self.speed_index.get() + 1) % SPEEDS.len();
        self.speed_index.set(index);
        let speed = SPEEDS[index];
        self.player.borrow_mut().set_speed(speed);
        self.speed_label.set_label(&format_speed(speed));
        self.update_speed_tooltip();
    }

    fn load_lyrics(&self, path: &Path, title: &str) {
        let lines = load_lrc_for_track(path, title);
        self.lyrics.replace(lines);
        self.lyric_index.set(usize::MAX);
        self.lyric_label.set_text("");
        self.lyric_revealer.set_reveal_child(false);
        self.rebuild_lyrics_panel();
        self.update_lyrics(self.player.borrow().position());
    }

    fn update_lyrics(&self, position: Duration) {
        let lyrics = self.lyrics.borrow();
        if lyrics.is_empty() {
            self.lyric_revealer.set_reveal_child(false);
            if !self.lyric_label.text().is_empty() {
                self.lyric_label.set_text("");
            }
            if self.lyric_index.get() != usize::MAX {
                self.lyric_index.set(usize::MAX);
                self.set_active_lyric(None);
            }
            return;
        }

        let all_timed = lyrics.iter().all(|line| line.time.is_some());
        let target = if all_timed {
            let next = lyrics
                .partition_point(|line| line.time.is_some_and(|time| time <= position));
            if next == 0 {
                usize::MAX
            } else {
                next - 1
            }
        } else {
            let total = self.current_duration.get().as_secs_f64();
            if total <= 0.0 {
                usize::MAX
            } else {
                let progress = (position.as_secs_f64() / total).clamp(0.0, 0.999_999);
                (progress * lyrics.len() as f64) as usize
            }
        };

        if target == self.lyric_index.get() {
            return;
        }
        self.lyric_index.set(target);
        if target == usize::MAX {
            self.lyric_label.set_text("");
            self.lyric_revealer.set_reveal_child(false);
        } else {
            let text = lyrics[target].text.replace('\n', "  ·  ");
            self.lyric_label.set_text(&text);
            self.lyric_revealer.set_reveal_child(true);
        }
        self.set_active_lyric(if target == usize::MAX { None } else { Some(target) });
    }

    fn rebuild_lyrics_panel(&self) {
        while let Some(child) = self.lyrics_list.first_child() {
            self.lyrics_list.remove(&child);
        }
        self.lyric_widgets.borrow_mut().clear();
        self.lyrics_scroll.vadjustment().set_value(0.0);

        self.lyrics_list.append(&self.lyric_pad_top);
        let lyrics = self.lyrics.borrow();
        if lyrics.is_empty() {
            self.lyric_pad_top.set_size_request(-1, 0);
            self.lyric_pad_bottom.set_size_request(-1, 0);
            let placeholder =
                gtk::Label::new(Some(self.lang().t("暂无歌词", "No lyrics")));
            placeholder.add_css_class("lyric-placeholder");
            self.lyrics_list.append(&placeholder);
            self.lyrics_list.append(&self.lyric_pad_bottom);
            return;
        }

        for line in lyrics.iter() {
            let label = gtk::Label::new(Some(&line.text));
            label.add_css_class("lyrics-line");
            label.set_wrap(true);
            label.set_wrap_mode(pango::WrapMode::WordChar);
            label.set_justify(gtk::Justification::Center);
            label.set_xalign(0.5);
            label.set_halign(gtk::Align::Fill);
            self.lyrics_list.append(&label);
            self.lyric_widgets.borrow_mut().push(label);
        }
        self.lyrics_list.append(&self.lyric_pad_bottom);
        self.update_lyric_pads();
    }

    fn set_active_lyric(&self, target: Option<usize>) {
        {
            let widgets = self.lyric_widgets.borrow();
            for (index, label) in widgets.iter().enumerate() {
                if Some(index) == target {
                    label.add_css_class("lyric-active");
                } else {
                    label.remove_css_class("lyric-active");
                }
            }
        }
        if let Some(index) = target {
            if self.lyrics_open.get() {
                self.scroll_lyrics_to(index);
            }
        }
    }

    fn scroll_lyrics_to_active(&self) {
        let index = self.lyric_index.get();
        if index != usize::MAX {
            self.scroll_lyrics_to(index);
        }
    }

    fn scroll_lyrics_to(&self, index: usize) {
        let label = {
            let widgets = self.lyric_widgets.borrow();
            widgets.get(index).cloned()
        };
        let Some(label) = label else {
            return;
        };
        let adjustment = self.lyrics_scroll.vadjustment();
        let viewport = adjustment.page_size();
        if viewport <= 0.0 || label.height() <= 0 {
            return;
        }
        let target = (f64::from(label.allocation().y()) + f64::from(label.height()) / 2.0
            - viewport / 2.0)
            .clamp(0.0, (adjustment.upper() - viewport).max(0.0));
        let start = adjustment.value();
        if (target - start).abs() < 1.0 {
            return;
        }

        let generation = self.lyric_scroll_gen.get().wrapping_add(1);
        self.lyric_scroll_gen.set(generation);
        let generation_cell = self.lyric_scroll_gen.clone();
        let steps = 12u32;
        let mut step = 0u32;

        glib::timeout_add_local(Duration::from_millis(15), move || {
            if generation_cell.get() != generation {
                return glib::ControlFlow::Break;
            }
            step += 1;
            let t = (step as f64 / steps as f64).min(1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            adjustment.set_value(start + (target - start) * eased);
            if t >= 1.0 {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn is_editing(&self) -> bool {
        gtk::prelude::GtkWindowExt::focus(&self.window)
            .map(|focus| {
                focus.is::<gtk::Entry>()
                    || focus.is::<gtk::SearchEntry>()
                    || focus.is::<gtk::TextView>()
            })
            .unwrap_or(false)
    }

    fn tick(&self) {
        if let Some(target) = self.pending_seek.get() {
            let due = self
                .last_seek_change
                .get()
                .map(|changed| changed.elapsed() >= SEEK_DEBOUNCE)
                .unwrap_or(true);
            if due {
                self.pending_seek.set(None);
                let duration = self.current_duration.get().as_secs_f64();
                let target = Duration::from_secs_f64(target.clamp(0.0, duration.max(0.0)));
                let _ = self.player.borrow_mut().seek(target);
                self.elapsed.set_label(&format_time(target));
                self.update_lyrics(target);
                return;
            }
        }

        let (state, finished, position) = {
            let player = self.player.borrow();
            (player.state(), player.finished(), player.position())
        };

        if state == PlaybackState::Stopped {
            return;
        }

        if finished {
            self.advance(true);
            return;
        }

        if self.pending_seek.get().is_none() {
            let duration = self.current_duration.get();
            let position = if duration.is_zero() {
                position
            } else {
                position.min(duration)
            };
            self.updating_seek.set(true);
            self.seek.set_value(position.as_secs_f64());
            self.updating_seek.set(false);
            self.elapsed.set_label(&format_time(position));
            self.update_lyrics(position);
        }
    }

    fn update_highlight(&self, old: Option<&Path>, new: Option<&Path>) {
        let rows = self.row_map.borrow();
        if let Some(old) = old {
            for (path, row) in rows.iter() {
                if path.as_path() == old {
                    row.remove_css_class("playing");
                }
            }
        }
        if let Some(new) = new {
            for (path, row) in rows.iter() {
                if path.as_path() == new {
                    row.add_css_class("playing");
                }
            }
        }
    }

    fn set_playing_icon(&self, playing: bool) {
        self.play_icon.set_icon_name(Some(if playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        }));
    }

    fn reset_seek_position(&self) {
        self.elapsed.set_label("0:00");
        self.updating_seek.set(true);
        self.seek.set_value(0.0);
        self.updating_seek.set(false);
    }

    fn show_error(&self, message: &str, detail: &str) {
        let dialog = gtk::Window::builder()
            .title(message)
            .modal(true)
            .transient_for(&self.window)
            .default_width(480)
            .build();
        dialog.add_css_class("dialog-window");
        dialog.add_css_class("error-dialog");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
        content.add_css_class("delete-content");

        let title = gtk::Label::new(Some(message));
        title.add_css_class("dialog-title");
        title.set_xalign(0.0);

        let body = gtk::Label::new(Some(detail));
        body.add_css_class("delete-body");
        body.set_wrap(true);
        body.set_xalign(0.0);
        body.set_selectable(true);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        actions.set_halign(gtk::Align::End);
        let ok = gtk::Button::with_label(self.lang().t("知道了", "Got it"));
        ok.add_css_class("suggested-action");
        ok.add_css_class("help-action");
        actions.append(&ok);

        content.append(&title);
        content.append(&body);
        content.append(&actions);
        dialog.set_child(Some(&content));

        let dialog_ok = dialog.clone();
        ok.connect_clicked(move |_| {
            dialog_ok.close();
        });

        dialog.present();
    }
}

fn format_time(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn format_seconds(seconds: f64) -> String {
    format_time(Duration::from_secs_f64(seconds.max(0.0)))
}

fn format_speed(speed: f32) -> String {
    if speed.fract().abs() < f32::EPSILON {
        format!("{speed:.1}x")
    } else {
        format!("{speed}x")
    }
}

fn load_lrc_for_track(audio: &Path, title: &str) -> Vec<LyricLine> {
    if let Some(path) = find_lrc(audio, title) {
        if let Some(text) = read_text_auto(&path) {
            let lines = parse_lyrics_text(&text);
            if !lines.is_empty() {
                return lines;
            }
        }
    }

    library::embedded_lyrics(audio)
        .map(|text| parse_lyrics_text(&text))
        .unwrap_or_default()
}

fn parse_lyrics_text(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    let mut offset_ms = 0i64;

    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if let Some(offset) = parse_lrc_offset(line) {
            offset_ms = offset;
            continue;
        }
        if line.is_empty() || is_lrc_metadata(line) || is_credit_line(line) {
            continue;
        }

        let stamps = find_timestamps(line);
        if stamps.is_empty() {
            let text = clean_lyric_text(line);
            if !text.is_empty() {
                lines.push(LyricLine { time: None, text });
            }
            continue;
        }

        let mut leading_count = 0;
        let mut cursor = 0;
        for &(start, end, _) in &stamps {
            if line[cursor..start].trim().is_empty() {
                leading_count += 1;
                cursor = end;
            } else {
                break;
            }
        }

        if leading_count == stamps.len() {
            let text = clean_lyric_text(&line[cursor..]);
            if text.is_empty() || is_credit_line(&text) {
                continue;
            }
            for &(_, _, time) in &stamps {
                lines.push(LyricLine {
                    time: Some(time),
                    text: text.clone(),
                });
            }
        } else {
            let text = clean_lyric_text(&strip_timestamps(line, &stamps));
            if text.is_empty() || is_credit_line(&text) {
                continue;
            }
            lines.push(LyricLine {
                time: Some(stamps[0].2),
                text,
            });
        }
    }

    if offset_ms != 0 {
        for line in &mut lines {
            if let Some(time) = line.time {
                let shifted = time.as_millis() as i64 + offset_ms;
                line.time = Some(Duration::from_millis(shifted.max(0) as u64));
            }
        }
    }

    merge_lyric_lines(lines)
}

fn parse_lrc_offset(line: &str) -> Option<i64> {
    let lower = line.to_ascii_lowercase();
    let rest = lower.strip_prefix("[offset:")?.strip_suffix(']')?;
    rest.trim().trim_start_matches('+').parse::<i64>().ok()
}

fn merge_lyric_lines(lines: Vec<LyricLine>) -> Vec<LyricLine> {
    if !lines.iter().any(|line| line.time.is_some()) {
        return lines;
    }

    // 先按文件顺序把无时间戳的行（通常是翻译）挂到上一句歌词上，
    // 再把同一时间戳的多行合并成一句，避免原文和译文被拆开。
    let mut merged: Vec<LyricLine> = Vec::new();
    for line in lines {
        match line.time {
            Some(time) => {
                if let Some(last) = merged.last_mut() {
                    if last.time == Some(time) {
                        append_lyric_text(&mut last.text, &line.text);
                        continue;
                    }
                }
                merged.push(line);
            }
            None => {
                if let Some(last) = merged.last_mut() {
                    append_lyric_text(&mut last.text, &line.text);
                }
            }
        }
    }

    merged.sort_by_key(|line| line.time);

    let mut result: Vec<LyricLine> = Vec::new();
    for line in merged {
        if let Some(last) = result.last_mut() {
            if last.time == line.time {
                append_lyric_text(&mut last.text, &line.text);
                continue;
            }
        }
        result.push(line);
    }
    result
}

fn append_lyric_text(target: &mut String, extra: &str) {
    if extra.is_empty() || target.lines().any(|line| line == extra) {
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(extra);
}

fn is_lrc_metadata(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    ["[ar:", "[ti:", "[al:", "[by:", "[offset:", "[re:", "[ve:"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn is_credit_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    let trimmed = lower.trim_start();
    const PREFIXES: &[&str] = &[
        "作词", "作曲", "编曲", "制作人", "监制", "出品", "发行", "混音", "母带", "录音",
        "和声", "配唱", "词：", "曲：", "词:", "曲:", "词 :", "曲 :", "lyrics by",
        "composed by", "written by", "produced by", "arranged by", "music by", "lyricist",
        "composer",
    ];
    PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn find_timestamps(line: &str) -> Vec<(usize, usize, Duration)> {
    let bytes = line.as_bytes();
    let mut stamps = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let open = bytes[index];
        if open == b'[' || open == b'<' {
            let close = if open == b'[' { b']' } else { b'>' };
            if let Some(offset) = bytes[index + 1..].iter().position(|byte| *byte == close) {
                let end = index + 1 + offset;
                if let Some(time) = parse_lrc_time(&line[index + 1..end]) {
                    stamps.push((index, end + 1, time));
                    index = end + 1;
                    continue;
                }
            }
        }
        index += 1;
    }

    stamps
}

fn strip_timestamps(line: &str, stamps: &[(usize, usize, Duration)]) -> String {
    let mut result = String::new();
    let mut cursor = 0;
    for &(start, end, _) in stamps {
        if start >= cursor {
            result.push_str(&line[cursor..start]);
        }
        cursor = end;
    }
    result.push_str(&line[cursor..]);
    result
}

fn clean_lyric_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    let mut result = String::with_capacity(collapsed.len());

    for (index, ch) in chars.iter().enumerate() {
        if *ch == ' '
            && index > 0
            && index + 1 < chars.len()
            && is_wide_char(chars[index - 1])
            && is_wide_char(chars[index + 1])
        {
            continue;
        }
        result.push(*ch);
    }

    result.trim().to_string()
}

fn is_wide_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{3000}'..='\u{303f}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}'
    )
}

fn find_lrc(audio: &Path, title: &str) -> Option<PathBuf> {
    let parent = audio.parent()?;
    let stem = audio.file_stem()?.to_string_lossy().to_string();

    let mut candidates = vec![
        parent.join(format!("{stem}.lrc")),
        parent.join(format!("{stem}.LRC")),
    ];

    if !title.is_empty() && title != stem && !title.contains('/') {
        candidates.push(parent.join(format!("{title}.lrc")));
        candidates.push(parent.join(format!("{title}.LRC")));
    }

    candidates.into_iter().find(|path| path.is_file())
}

fn read_text_auto(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = match String::from_utf8(bytes.clone()) {
        Ok(text) => text,
        Err(_) => encoding_rs::GB18030.decode(&bytes).0.into_owned(),
    };
    Some(text.trim_start_matches('\u{feff}').to_string())
}

fn parse_lrc_time(tag: &str) -> Option<Duration> {
    let (minutes, seconds) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let seconds: f64 = seconds.trim().replace(',', ".").parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(minutes as f64 * 60.0 + seconds))
}

fn config_path() -> Option<PathBuf> {
    let dir = glib::user_config_dir().join("rustmusic");
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("config"))
}

fn load_config() -> SavedState {
    let mut saved = SavedState::default();
    let Some(path) = config_path() else {
        return saved;
    };
    let Ok(text) = fs::read_to_string(path) else {
        return saved;
    };

    for line in text.lines() {
        if let Some(value) = line.strip_prefix("folder=") {
            let folder = PathBuf::from(value);
            if folder.is_dir() {
                saved.folder = Some(folder);
            }
        } else if let Some(value) = line.strip_prefix("volume=") {
            if let Ok(volume) = value.parse::<f32>() {
                saved.volume = Some(volume.clamp(0.0, 100.0));
            }
        } else if let Some(value) = line.strip_prefix("favorite=") {
            saved.favorites.insert(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("removed=") {
            saved.removed.insert(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("lang=") {
            saved.lang = Lang::from_code(value);
        }
    }
    saved
}

fn save_config(
    folder: Option<&Path>,
    volume: f32,
    favorites: &HashSet<PathBuf>,
    removed: &HashSet<PathBuf>,
    lang: Lang,
) {
    let Some(path) = config_path() else {
        return;
    };
    let mut text = String::new();
    if let Some(folder) = folder {
        text.push_str("folder=");
        text.push_str(&folder.display().to_string());
        text.push('\n');
    }
    text.push_str(&format!("volume={:.0}\n", volume * 100.0));
    text.push_str(&format!("lang={}\n", lang.code()));

    let mut favorite_paths: Vec<_> = favorites.iter().collect();
    favorite_paths.sort();
    for path in favorite_paths {
        text.push_str("favorite=");
        text.push_str(&path.display().to_string());
        text.push('\n');
    }

    let mut removed_paths: Vec<_> = removed.iter().collect();
    removed_paths.sort();
    for path in removed_paths {
        text.push_str("removed=");
        text.push_str(&path.display().to_string());
        text.push('\n');
    }

    let _ = fs::write(path, text);
}

enum TrayCommand {
    Show(Option<String>),
    Quit,
}

struct RustMusicTray {
    tx: mpsc::Sender<TrayCommand>,
    icons: Vec<ksni::Icon>,
    lang: Lang,
}

impl ksni::Tray for RustMusicTray {
    fn id(&self) -> String {
        "io.github.rustmusic.RustMusic".into()
    }

    fn title(&self) -> String {
        "RustMusic".into()
    }

    fn icon_name(&self) -> String {
        "io.github.rustmusic.RustMusic".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let token = ksni::current_xdg_activation_token();
        let _ = self.tx.send(TrayCommand::Show(token));
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: self.lang.t("显示 RustMusic", "Show RustMusic").into(),
                activate: Box::new(|tray: &mut RustMusicTray| {
                    let token = ksni::current_xdg_activation_token();
                    let _ = tray.tx.send(TrayCommand::Show(token));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: self.lang.t("退出", "Quit").into(),
                activate: Box::new(|tray: &mut RustMusicTray| {
                    let _ = tray.tx.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn tray_icons() -> Vec<ksni::Icon> {
    use gdk_pixbuf::prelude::*;

    let Some(pixbuf) = scaled_logo(32) else {
        return Vec::new();
    };
    let channels = pixbuf.n_channels() as usize;
    let bytes = pixbuf.read_pixel_bytes();
    let raw = bytes.as_ref();
    let mut argb = Vec::with_capacity(raw.len() / channels * 4);

    for pixel in raw.chunks_exact(channels) {
        let (r, g, b, a) = if channels >= 4 {
            (pixel[0], pixel[1], pixel[2], pixel[3])
        } else {
            (pixel[0], pixel[1], pixel[2], 255)
        };
        argb.extend_from_slice(&[a, r, g, b]);
    }

    vec![ksni::Icon {
        width: pixbuf.width(),
        height: pixbuf.height(),
        data: argb,
    }]
}

fn spawn_tray(ui: Rc<Ui>, app: gtk::Application) {
    let (tx, rx) = mpsc::channel::<TrayCommand>();
    let icons = tray_icons();
    let lang = ui.lang();

    // 托盘注册依赖桌面环境提供的 StatusNotifierWatcher，可能阻塞或超时，
    // 放到后台线程，避免拖慢主界面显示。
    std::thread::spawn(move || {
        let tray = RustMusicTray { tx, icons, lang };
        match tray.spawn() {
            Ok(handle) => std::mem::forget(handle),
            Err(err) => eprintln!("无法创建系统托盘：{err}"),
        }
    });

    glib::timeout_add_local(Duration::from_millis(100), move || loop {
        match rx.try_recv() {
            Ok(TrayCommand::Show(token)) => {
                if let Some(token) = token {
                    ui.window.set_startup_id(&token);
                }
                ui.window.unminimize();
                ui.window.present();
            }
            Ok(TrayCommand::Quit) => {
                ui.save_state();
                app.quit();
                return glib::ControlFlow::Break;
            }
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
        }
    });
}
