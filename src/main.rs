#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod archive;
mod config;
mod managers;

use std::{
	cell::RefCell,
	collections::{HashMap, HashSet},
	fmt::Write as _,
	fs,
	path::{Path, PathBuf},
	rc::Rc,
	sync::mpsc::{self, Receiver, Sender},
	time::{SystemTime, UNIX_EPOCH},
};

use config::{Config, Manager, Package};
use managers::{Hit, Msg};
use wxdragon::prelude::*;

type Then = Box<dyn FnOnce(&Rc<App>)>;
type Done = Box<dyn FnOnce(&Rc<App>, bool)>;

struct State {
	config: Config,
	installed: Vec<Manager>,
	/// The manager shown in the packages list.
	current: Manager,
	/// Row in the packages list -> index into config.packages.
	rows: Vec<usize>,
	/// Packages ticked for install/upgrade, kept across manager switches.
	checked: HashSet<(Manager, String)>,
	next_job: u64,
	busy: bool,
	jobs: HashMap<u64, Done>,
	search_token: u64,
	search_cb: Option<Box<dyn Fn(Vec<Hit>)>>,
}

struct App {
	frame: Frame,
	managers_list: ListBox,
	packages: ListBox,
	live_region: StaticText,
	packages_label: StaticText,
	log: TextCtrl,
	tx: Sender<Msg>,
	state: RefCell<State>,
}

const ID_SEARCH: i32 = ID_HIGHEST + 2;
const ID_MANUAL: i32 = ID_HIGHEST + 3;
const ID_INSTALL_SELECTED: i32 = ID_HIGHEST + 4;
const ID_UPGRADE_SELECTED: i32 = ID_HIGHEST + 5;
const ID_UPGRADE_ALL: i32 = ID_HIGHEST + 6;
const ID_EDIT: i32 = ID_HIGHEST + 7;
const ID_DELETE: i32 = ID_HIGHEST + 8;
// Context-menu only: act on the highlighted package.
const ID_INSTALL_ONE: i32 = ID_HIGHEST + 9;
const ID_UPGRADE_ONE: i32 = ID_HIGHEST + 10;
const ID_TOGGLE: i32 = ID_HIGHEST + 11;
const ID_INSTALL_MANAGER: i32 = ID_HIGHEST + 12;
const ID_SETTINGS: i32 = ID_HIGHEST + 13;
const ID_CUSTOM: i32 = ID_HIGHEST + 14;

thread_local! {
	static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
	static RX: RefCell<Option<Receiver<Msg>>> = const { RefCell::new(None) };
}

/// Delivers queued worker messages on the UI thread. Nothing is borrowed while a message is
/// handled, because handling one may open a modal dialog that pumps events and re-enters here.
pub fn drain() {
	loop {
		let msg = RX.with(|rx| rx.borrow().as_ref().and_then(|rx| rx.try_recv().ok()));
		let Some(msg) = msg else { break };
		let Some(app) = APP.with(|a| a.borrow().clone()) else { break };
		handle_msg(&app, msg);
	}
}

// ---------------------------------------------------------------- helpers

fn confirm(parent: &impl WxWidget, msg: &str, title: &str) -> bool {
	let dlg = MessageDialog::builder(parent, msg, title)
		.with_style(MessageDialogStyle::YesNo | MessageDialogStyle::IconQuestion)
		.build();
	dlg.show_modal() == ID_YES
}

fn info(parent: &impl WxWidget, msg: &str, title: &str) {
	let dlg = MessageDialog::builder(parent, msg, title)
		.with_style(MessageDialogStyle::OK | MessageDialogStyle::IconInformation)
		.build();
	dlg.show_modal();
}

fn log(app: &App, text: &str) {
	app.log.append_text(&format!("{text}\n"));
}

fn is_installed(app: &App, m: Manager) -> bool {
	m == Manager::Custom || app.state.borrow().installed.contains(&m)
}

// ---------------------------------------------------------------- jobs

/// Starts a PowerShell job. Returns false if another job is running.
fn start_job(app: &Rc<App>, script: String, done: Option<Done>) -> bool {
	let Some(id) = begin_job(app, done) else { return false };
	managers::run_job(id, script, app.tx.clone());
	true
}

/// Runs Rust code on a worker thread as the current job. Returns false if another job is running.
fn start_task(app: &Rc<App>, work: impl FnOnce(&Sender<Msg>) -> bool + Send + 'static, done: Done) -> bool {
	let Some(id) = begin_job(app, Some(done)) else { return false };
	managers::run_task(id, app.tx.clone(), work);
	true
}

/// Marks the app busy and registers the completion callback; `None` if already busy.
fn begin_job(app: &Rc<App>, done: Option<Done>) -> Option<u64> {
	let id = {
		let mut st = app.state.borrow_mut();
		if st.busy {
			drop(st);
			log(app, "Another task is still running; please wait for it to finish.");
			return None;
		}
		st.busy = true;
		st.next_job += 1;
		let id = st.next_job;
		if let Some(d) = done {
			st.jobs.insert(id, d);
		}
		id
	};
	app.frame.set_status_text("Working...", 0);
	Some(id)
}

fn redetect(app: &Rc<App>) {
	let found = managers::detect();
	app.state.borrow_mut().installed = found;
	refresh_tree(app);
}

fn install_manager(app: &Rc<App>, m: Manager, done: Done) {
	let (winget, scoop) = {
		let st = app.state.borrow();
		(st.installed.contains(&Manager::Winget), st.installed.contains(&Manager::Scoop))
	};
	log(app, &format!("=== Installing {} ===", m.label()));
	let script = managers::install_manager_script(m, winget, scoop);
	let wrapped: Done = Box::new(move |app, _ok| {
		redetect(app);
		let now = is_installed(app, m);
		if now {
			log(app, &format!("{} is now installed.", m.label()));
		} else {
			log(app, &format!("{} still was not detected. You may need to restart Toolsy.", m.label()));
		}
		done(app, now);
	});
	start_job(app, script, Some(wrapped));
}

/// Makes sure `m` exists (offering to install it), then runs `then`.
fn ensure_manager(app: &Rc<App>, m: Manager, then: Then) {
	if is_installed(app, m) {
		return then(app);
	}
	let msg = format!("{} is not installed. Do you want to install it now?", m.label());
	if confirm(&app.frame, &msg, &format!("{} not installed", m.label())) {
		install_manager(
			app,
			m,
			Box::new(move |app, ok| {
				if ok {
					then(app);
				}
			}),
		);
	}
}

fn ensure_all(app: &Rc<App>, mut list: Vec<Manager>, then: Then) {
	if list.is_empty() {
		return then(app);
	}
	let m = list.remove(0);
	ensure_manager(app, m, Box::new(move |app| ensure_all(app, list, then)));
}

fn first_run(app: &Rc<App>, mut missing: Vec<Manager>) {
	if missing.is_empty() {
		let mut st = app.state.borrow_mut();
		st.config.first_run_done = true;
		let _ = st.config.save();
		return;
	}
	let m = missing.remove(0);
	let msg = format!("{} is not installed. Do you want to install it now?", m.label());
	if confirm(&app.frame, &msg, "Toolsy - first run") {
		install_manager(app, m, Box::new(move |app, _| first_run(app, missing)));
	} else {
		first_run(app, missing);
	}
}

enum Action {
	Install,
	Upgrade,
}

fn run_packages(app: &Rc<App>, pkgs: Vec<Package>, action: Action) {
	if pkgs.is_empty() {
		info(&app.frame, "Select the packages you want first.", "Nothing selected");
		return;
	}
	let (custom, pkgs): (Vec<Package>, Vec<Package>) = pkgs.into_iter().partition(|p| p.manager == Manager::Custom);
	let mut needed: Vec<Manager> = Vec::new();
	for p in &pkgs {
		if !needed.contains(&p.manager) {
			needed.push(p.manager);
		}
	}
	ensure_all(
		app,
		needed,
		Box::new(move |app| {
			if pkgs.is_empty() {
				return custom_queue(app, custom, true);
			}
			let mut script = String::new();
			for p in &pkgs {
				let cmd = match action {
					Action::Install => managers::install_cmd(p),
					Action::Upgrade => managers::upgrade_cmd(p),
				};
				let _ = writeln!(
					script,
					"Write-Host '>>> {}'; {cmd}; if($LASTEXITCODE -ne 0){{ Write-Host 'FAILED (exit code' $LASTEXITCODE ')' }}",
					p.name.replace('\'', "''")
				);
			}
			let done: Done = Box::new(move |app, ok| {
				if custom.is_empty() {
					log(app, if ok { "=== Finished ===" } else { "=== Finished with errors ===" });
				} else {
					custom_queue(app, custom, ok);
				}
			});
			start_job(app, script, Some(done));
		}),
	);
}

fn upgrade_all(app: &Rc<App>, m: Manager) {
	if m == Manager::Custom {
		info(
			&app.frame,
			"Custom packages have no \"upgrade all\". Select them and use Upgrade Selected.",
			"Upgrade all",
		);
		return;
	}
	ensure_manager(
		app,
		m,
		Box::new(move |app| {
			let script = format!("Write-Host '>>> upgrade all ({})'; {}", m.label(), managers::upgrade_all_cmd(m));
			let done: Done = Box::new(|app, _| log(app, "=== Upgrade finished ==="));
			start_job(app, script, Some(done));
		}),
	);
}

// ---------------------------------------------------------------- custom packages

/// Folder a custom package is extracted or saved to: its own subfolder of the extract location.
fn custom_dest(app: &App, p: &Package) -> PathBuf {
	let base = app.state.borrow().config.extract_dir.clone();
	let base = if base.trim().is_empty() { config::default_extract_dir() } else { base };
	PathBuf::from(base).join(archive::sanitize(&p.name))
}

fn show_path(path: &Path) -> String {
	std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()).display().to_string()
}

fn is_http_url(url: &str) -> bool {
	let lower = url.to_ascii_lowercase();
	lower.starts_with("https://") || lower.starts_with("http://")
}

/// File name at the end of a URL, for saving files that are neither installers nor archives.
fn url_file_name(url: &str) -> String {
	let path = url.split(['?', '#']).next().unwrap_or(url);
	let last = path.rsplit('/').next().unwrap_or("");
	if last.is_empty() { "download.bin".to_string() } else { archive::sanitize(last) }
}

/// Downloads and installs the custom packages one after another, asking before each run or extract.
fn custom_queue(app: &Rc<App>, mut queue: Vec<Package>, ok: bool) {
	if queue.is_empty() {
		log(app, if ok { "=== Finished ===" } else { "=== Finished with errors ===" });
		return;
	}
	let p = queue.remove(0);
	log(app, &format!(">>> {}", p.name));
	if !is_http_url(&p.id) {
		log(app, "FAILED: the address must start with http:// or https://");
		return custom_queue(app, queue, false);
	}
	let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
	let dir = std::env::temp_dir().join(format!("toolsy-{}-{nanos}", std::process::id()));
	if let Err(e) = fs::create_dir_all(&dir) {
		log(app, &format!("FAILED: could not create a temporary folder: {e}"));
		return custom_queue(app, queue, false);
	}
	log(app, &format!("Downloading {}", p.id));
	let script = managers::download_script(&p.id, &dir.join("download.bin"));
	let cleanup = dir.clone();
	let started = start_job(
		app,
		script,
		Some(Box::new(move |app, downloaded| {
			if downloaded {
				custom_downloaded(app, &p, dir, queue, ok);
			} else {
				log(app, "FAILED: the download did not complete");
				custom_next(app, &dir, queue, false);
			}
		})),
	);
	if !started {
		let _ = fs::remove_dir_all(cleanup);
	}
}

/// Removes the temporary download folder and moves on to the next package.
fn custom_next(app: &Rc<App>, dir: &Path, queue: Vec<Package>, ok: bool) {
	let _ = fs::remove_dir_all(dir);
	custom_queue(app, queue, ok);
}

fn custom_skip(app: &Rc<App>, dir: &Path, queue: Vec<Package>, ok: bool) {
	log(app, "Skipped.");
	custom_next(app, dir, queue, ok);
}

fn custom_downloaded(app: &Rc<App>, p: &Package, dir: PathBuf, queue: Vec<Package>, ok: bool) {
	let file = dir.join("download.bin");
	let kind = match archive::sniff(&file) {
		Ok(k) => k,
		Err(e) => {
			log(app, &format!("FAILED: could not read the download: {e}"));
			return custom_next(app, &dir, queue, false);
		}
	};
	let dest = custom_dest(app, p);
	match kind {
		archive::Kind::Exe | archive::Kind::Msi => {
			let msi = kind == archive::Kind::Msi;
			let what = if msi { "MSI installer" } else { "installer" };
			let msg = format!("Downloaded the {what} for \"{}\".\nRun it now?", p.name);
			if !confirm(&app.frame, &msg, "Run installer") {
				return custom_skip(app, &dir, queue, ok);
			}
			let runnable = dir.join(if msi { "setup.msi" } else { "setup.exe" });
			if let Err(e) = fs::rename(&file, &runnable) {
				log(app, &format!("FAILED: could not prepare the installer: {e}"));
				return custom_next(app, &dir, queue, false);
			}
			let script = managers::run_installer_script(&runnable, msi);
			let cleanup = dir.clone();
			let done: Done = Box::new(move |app, ran| custom_next(app, &dir, queue, ok && ran));
			if !start_job(app, script, Some(done)) {
				let _ = fs::remove_dir_all(cleanup);
			}
		}
		k if k.is_archive() => {
			let msg = format!("Downloaded \"{}\".\nExtract it to {}?", p.name, show_path(&dest));
			if !confirm(&app.frame, &msg, "Extract archive") {
				return custom_skip(app, &dir, queue, ok);
			}
			let single = url_file_name(&p.id);
			let single = single.rsplit_once('.').map_or(single.as_str(), |(stem, _)| stem).to_string();
			let work = move |tx: &Sender<Msg>| match archive::extract(&file, kind, &dest, &single) {
				Ok(n) => {
					managers::emit(tx, Msg::Line(format!("Extracted {n} file(s) to {}", show_path(&dest))));
					true
				}
				Err(e) => {
					managers::emit(tx, Msg::Line(format!("FAILED: {e}")));
					false
				}
			};
			let cleanup = dir.clone();
			let done: Done = Box::new(move |app, extracted| custom_next(app, &dir, queue, ok && extracted));
			if !start_task(app, work, done) {
				let _ = fs::remove_dir_all(cleanup);
			}
		}
		_ => {
			let msg = format!(
				"Downloaded \"{}\", but it is not an installer or a supported archive.\nSave it to {} instead?",
				p.name,
				show_path(&dest)
			);
			if !confirm(&app.frame, &msg, "Unknown file type") {
				return custom_skip(app, &dir, queue, ok);
			}
			let target = dest.join(url_file_name(&p.id));
			match fs::create_dir_all(&dest).and_then(|()| fs::copy(&file, &target)) {
				Ok(_) => {
					log(app, &format!("Saved to {}", show_path(&target)));
					custom_next(app, &dir, queue, ok);
				}
				Err(e) => {
					log(app, &format!("FAILED: could not save the file: {e}"));
					custom_next(app, &dir, queue, false);
				}
			}
		}
	}
}

// ---------------------------------------------------------------- lists

fn manager_label(app: &App, m: Manager) -> String {
	if is_installed(app, m) { m.label().to_string() } else { format!("{} (not installed)", m.label()) }
}

fn package_label(p: &Package, selected: bool) -> String {
	let mut label = if p.name.is_empty() { p.id.clone() } else { p.name.clone() };
	if selected {
		label.push_str(", selected");
	}
	label
}

fn row_index(row: usize) -> u32 {
	u32::try_from(row).unwrap_or(u32::MAX)
}

fn manager_index(app: &App, m: Manager) -> u32 {
	app.state.borrow().config.managers().iter().position(|x| *x == m).map_or(0, row_index)
}

/// Position of `m` among the real package managers (for the manager choice boxes).
fn real_index(m: Manager) -> u32 {
	Manager::REAL.iter().position(|x| *x == m).map_or(0, row_index)
}

fn announce(app: &App, message: &str) {
	live_region::announce(app.live_region, message);
}

/// Selects or deselects the package in `row` (a list row) for install/upgrade.
fn toggle_package(app: &Rc<App>, row: usize) {
	let (label, now) = {
		let mut st = app.state.borrow_mut();
		let Some(&idx) = st.rows.get(row) else { return };
		let p = st.config.packages[idx].clone();
		let key = (p.manager, p.id.clone());
		let now = !st.checked.remove(&key);
		if now {
			st.checked.insert(key);
		}
		(package_label(&p, now), now)
	};
	app.packages.set_string(row_index(row), &label);
	app.packages.set_selection(row_index(row), true);
	announce(app, if now { "selected" } else { "not selected" });
}

fn manager_at(app: &App, row: usize) -> Option<Manager> {
	app.state.borrow().config.managers().get(row).copied()
}

fn refresh_managers(app: &App) {
	let current = app.state.borrow().current;
	app.managers_list.clear();
	for m in app.state.borrow().config.managers() {
		app.managers_list.append(&manager_label(app, m));
	}
	app.managers_list.set_selection(manager_index(app, current), true);
}

/// Rebuilds both lists from the state.
fn refresh_tree(app: &Rc<App>) {
	refresh_managers(app);
	app.packages_label.set_label(&format!("{} packages", app.state.borrow().current.label()));
	refresh_packages(app);
}

/// Moves the current manager one row up (`delta == -1`) or down (`delta == 1`) and saves the order.
fn move_manager(app: &Rc<App>, delta: isize) {
	let position = {
		let mut st = app.state.borrow_mut();
		let mut order = st.config.managers();
		let Some(from) = order.iter().position(|m| *m == st.current) else { return };
		let Some(to) = from.checked_add_signed(delta).filter(|&to| to < order.len()) else { return };
		order.swap(from, to);
		st.config.manager_order = order;
		to + 1
	};
	refresh_managers(app);
	save_config(app);
	announce(app, &format!("moved to position {position} of {}", Manager::ALL.len()));
}

fn refresh_packages(app: &Rc<App>) {
	let mut st = app.state.borrow_mut();
	let st = &mut *st;
	app.packages.clear();
	st.rows.clear();
	let mut order: Vec<usize> =
		st.config.packages.iter().enumerate().filter(|(_, p)| p.manager == st.current).map(|(i, _)| i).collect();
	if st.config.sort_alphabetically {
		let key = |i: usize| {
			let p = &st.config.packages[i];
			(if p.name.is_empty() { p.id.clone() } else { p.name.clone() }).to_lowercase()
		};
		order.sort_by_cached_key(|&i| key(i));
	}
	for idx in order {
		let p = &st.config.packages[idx];
		app.packages.append(&package_label(p, st.checked.contains(&(p.manager, p.id.clone()))));
		st.rows.push(idx);
	}
}

fn selected(app: &App) -> (Option<usize>, Option<Manager>) {
	let st = app.state.borrow();
	let idx = app.packages.get_selection().and_then(|i| st.rows.get(i as usize).copied());
	(idx, Some(st.current))
}

fn selected_packages(app: &App) -> Vec<Package> {
	let st = app.state.borrow();
	st.config.packages.iter().filter(|p| st.checked.contains(&(p.manager, p.id.clone()))).cloned().collect()
}

fn save_config(app: &Rc<App>) {
	if let Err(e) = app.state.borrow().config.save() {
		info(&app.frame, &format!("Could not write toolsy.toml:\n{e}"), "Save failed");
	}
}

// ---------------------------------------------------------------- dialogs

/// Add/edit dialog. Returns the edited package when OK is pressed.
fn package_dialog(app: &Rc<App>, title: &str, initial: Option<&Package>, default_mgr: Manager) -> Option<Package> {
	let dlg = Dialog::builder(&app.frame, title).with_size(520, 300).build();
	// Each label is created right before its field, so screen readers pair them up.
	let grid = FlexGridSizer::builder(0, 2).with_vgap(8).with_hgap(8).build();
	grid.add_growable_col(1, 1);
	macro_rules! row {
		($label:expr, $field:expr) => {{
			let st = StaticText::builder(&dlg).with_label($label).build();
			let field = $field;
			grid.add(&st, 0, SizerFlag::AlignCenterVertical, 0);
			grid.add(&field, 1, SizerFlag::Expand, 0);
			field
		}};
	}
	let mgr = row!(
		"Package &manager:",
		Choice::builder(&dlg).with_choices(Manager::REAL.iter().map(|m| m.label().to_string()).collect()).build()
	);
	let start = initial.map_or(default_mgr, |p| p.manager);
	mgr.set_selection(real_index(start));
	let name =
		row!("Display &name:", TextCtrl::builder(&dlg).with_value(initial.map_or("", |p| p.name.as_str())).build());
	let id =
		row!("Package &identifier:", TextCtrl::builder(&dlg).with_value(initial.map_or("", |p| p.id.as_str())).build());
	let args = row!(
		"&Parameters (optional):",
		TextCtrl::builder(&dlg).with_value(initial.map_or("", |p| p.args.as_str())).build()
	);
	let ok = Button::builder(&dlg).with_label("OK").build();
	let cancel = Button::builder(&dlg).with_label("Cancel").build();
	let hint = StaticText::builder(&dlg)
		.with_label("Parameters are passed to the package manager, e.g. winget: --architecture x64")
		.build();
	let buttons = BoxSizer::builder(Orientation::Horizontal).build();
	buttons.add_stretch_spacer(1);
	buttons.add(&ok, 0, SizerFlag::All, 4);
	buttons.add(&cancel, 0, SizerFlag::All, 4);
	let top = BoxSizer::builder(Orientation::Vertical).build();
	top.add_sizer(&grid, 0, SizerFlag::Expand | SizerFlag::All, 12);
	top.add(&hint, 0, SizerFlag::All, 12);
	top.add_stretch_spacer(1);
	top.add_sizer(&buttons, 0, SizerFlag::Expand | SizerFlag::All, 8);
	dlg.set_sizer(top, true);

	ok.on_click(move |_| dlg.end_modal(ID_OK));
	cancel.on_click(move |_| dlg.end_modal(ID_CANCEL));
	dlg.set_affirmative_id(ID_OK);
	dlg.set_escape_id(ID_CANCEL);

	loop {
		if dlg.show_modal() != ID_OK {
			dlg.destroy();
			return None;
		}
		let m = Manager::REAL[mgr.get_selection().unwrap_or(0) as usize];
		let pid = id.get_value().trim().to_string();
		let pname = name.get_value().trim().to_string();
		if pid.is_empty() {
			info(&dlg, "The package identifier is required.", "Missing identifier");
			continue;
		}
		let result = Package {
			manager: m,
			name: if pname.is_empty() { pid.clone() } else { pname },
			id: pid,
			args: args.get_value().trim().to_string(),
		};
		dlg.destroy();
		return Some(result);
	}
}

/// Add/edit dialog for custom (download URL) packages.
fn custom_dialog(app: &Rc<App>, title: &str, initial: Option<&Package>) -> Option<Package> {
	let dlg = Dialog::builder(&app.frame, title).with_size(560, 280).build();
	let grid = FlexGridSizer::builder(0, 2).with_vgap(8).with_hgap(8).build();
	grid.add_growable_col(1, 1);
	macro_rules! row {
		($label:expr, $field:expr) => {{
			let st = StaticText::builder(&dlg).with_label($label).build();
			let field = $field;
			grid.add(&st, 0, SizerFlag::AlignCenterVertical, 0);
			grid.add(&field, 1, SizerFlag::Expand, 0);
			field
		}};
	}
	let name =
		row!("Display &name:", TextCtrl::builder(&dlg).with_value(initial.map_or("", |p| p.name.as_str())).build());
	let url = row!("Download &URL:", TextCtrl::builder(&dlg).with_value(initial.map_or("", |p| p.id.as_str())).build());
	let ok = Button::builder(&dlg).with_label("OK").build();
	let cancel = Button::builder(&dlg).with_label("Cancel").build();
	let hint = StaticText::builder(&dlg)
		.with_label(
			"The URL can be an exe or msi installer, or an archive (zip, 7z, tar, gz, bz2, xz, zst). \
			 Toolsy asks before running or extracting it. Archives go to a subfolder of the extract location in Settings.",
		)
		.build();
	hint.wrap(520);
	let buttons = BoxSizer::builder(Orientation::Horizontal).build();
	buttons.add_stretch_spacer(1);
	buttons.add(&ok, 0, SizerFlag::All, 4);
	buttons.add(&cancel, 0, SizerFlag::All, 4);
	let top = BoxSizer::builder(Orientation::Vertical).build();
	top.add_sizer(&grid, 0, SizerFlag::Expand | SizerFlag::All, 12);
	top.add(&hint, 0, SizerFlag::All, 12);
	top.add_stretch_spacer(1);
	top.add_sizer(&buttons, 0, SizerFlag::Expand | SizerFlag::All, 8);
	dlg.set_sizer(top, true);

	ok.on_click(move |_| dlg.end_modal(ID_OK));
	cancel.on_click(move |_| dlg.end_modal(ID_CANCEL));
	dlg.set_affirmative_id(ID_OK);
	dlg.set_escape_id(ID_CANCEL);

	loop {
		if dlg.show_modal() != ID_OK {
			dlg.destroy();
			return None;
		}
		let purl = url.get_value().trim().to_string();
		let pname = name.get_value().trim().to_string();
		if !is_http_url(&purl) {
			info(&dlg, "Enter a download URL that starts with http:// or https://.", "Invalid URL");
			continue;
		}
		let result = Package {
			manager: Manager::Custom,
			name: if pname.is_empty() { url_file_name(&purl) } else { pname },
			id: purl,
			args: String::new(),
		};
		dlg.destroy();
		return Some(result);
	}
}

fn hit_label(h: &Hit, selected: bool) -> String {
	let ver = if h.version.is_empty() { String::new() } else { format!(", version {}", h.version) };
	format!("{}, {}{}{}", h.name, h.id, ver, if selected { ", selected" } else { "" })
}

/// Search dialog: search a manager, check results, add them to the config (no install).
#[allow(clippy::too_many_lines)]
fn search_dialog(app: &Rc<App>, default_mgr: Manager) {
	let dlg = Dialog::builder(&app.frame, "Search packages").with_size(640, 520).build();
	let mgr_label = StaticText::builder(&dlg).with_label("Package &manager:").build();
	let mgr = Choice::builder(&dlg).with_choices(Manager::REAL.iter().map(|m| m.label().to_string()).collect()).build();
	mgr.set_selection(real_index(default_mgr));
	let query_label = StaticText::builder(&dlg).with_label("&Search for:").build();
	let query = TextCtrl::builder(&dlg).with_style(TextCtrlStyle::ProcessEnter).build();
	let go = Button::builder(&dlg).with_label("Search").build();
	let status = StaticText::builder(&dlg)
		.with_label("Search, select what you want, then add it to your config. Nothing is installed yet.")
		.build();
	let list_label = StaticText::builder(&dlg).with_label("Search &results").build();
	let list = ListBox::builder(&dlg).build();
	// Made after the list, so no screen reader takes it for its label.
	let live = StaticText::builder(&dlg).with_label("").with_size(Size::new(0, 0)).build();
	live.show(false);
	let add = Button::builder(&dlg).with_label("Add selected to config").build();
	let close = Button::builder(&dlg).with_label("Close").build();

	let row = BoxSizer::builder(Orientation::Horizontal).build();
	row.add(&mgr_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::Left, 4);
	row.add(&mgr, 0, SizerFlag::All, 4);
	row.add(&query_label, 0, SizerFlag::AlignCenterVertical | SizerFlag::Left, 4);
	row.add(&query, 1, SizerFlag::All | SizerFlag::Expand, 4);
	row.add(&go, 0, SizerFlag::All, 4);
	let bottom = BoxSizer::builder(Orientation::Horizontal).build();
	bottom.add_stretch_spacer(1);
	bottom.add(&add, 0, SizerFlag::All, 4);
	bottom.add(&close, 0, SizerFlag::All, 4);
	let top = BoxSizer::builder(Orientation::Vertical).build();
	top.add_sizer(&row, 0, SizerFlag::Expand | SizerFlag::All, 6);
	top.add(&status, 0, SizerFlag::All, 8);
	top.add(&list_label, 0, SizerFlag::Left, 8);
	top.add(&list, 1, SizerFlag::Expand | SizerFlag::All, 8);
	top.add_sizer(&bottom, 0, SizerFlag::Expand | SizerFlag::All, 6);
	dlg.set_sizer(top, true);

	let hits: Rc<RefCell<Vec<Hit>>> = Rc::new(RefCell::new(Vec::new()));
	let picked: Rc<RefCell<HashSet<usize>>> = Rc::new(RefCell::new(HashSet::new()));

	let do_search = {
		let app = app.clone();
		let hits = hits.clone();
		let picked = picked.clone();
		move || {
			let q = query.get_value().trim().to_string();
			if q.is_empty() {
				return;
			}
			let m = Manager::REAL[mgr.get_selection().unwrap_or(0) as usize];
			if !is_installed(&app, m) {
				let msg = format!("{} is not installed. Do you want to install it now?", m.label());
				if confirm(&dlg, &msg, &format!("{} not installed", m.label())) {
					install_manager(&app, m, Box::new(|_, _| {}));
				}
				return;
			}
			status.set_label(&format!("Searching {} for \"{q}\"...", m.label()));
			list.clear();
			hits.borrow_mut().clear();
			picked.borrow_mut().clear();
			let token = {
				let mut st = app.state.borrow_mut();
				st.search_token += 1;
				let token = st.search_token;
				let hits = hits.clone();
				st.search_cb = Some(Box::new(move |found: Vec<Hit>| {
					list.clear();
					for h in &found {
						list.append(&hit_label(h, false));
					}
					status.set_label(&format!("{} result(s). Select the ones you want with Space.", found.len()));
					*hits.borrow_mut() = found;
				}));
				token
			};
			let tx = app.tx.clone();
			std::thread::spawn(move || {
				let found = managers::search(m, &q);
				managers::emit(&tx, Msg::Search { token, hits: found });
			});
		}
	};
	let do_search = Rc::new(do_search);
	{
		let f = do_search.clone();
		go.on_click(move |_| f());
		let f = do_search;
		query.on_text_enter(move |_| f());
	}
	{
		let hits = hits.clone();
		let picked = picked.clone();
		list.on_key_down(move |event| {
			if let WindowEventData::Keyboard(ref key) = event
				&& key.get_key_code() == Some(WXK_SPACE)
			{
				if let Some(row) = list.get_selection() {
					let row = row as usize;
					let Some(hit) = hits.borrow().get(row).cloned() else { return };
					let now = !picked.borrow_mut().remove(&row);
					if now {
						picked.borrow_mut().insert(row);
					}
					list.set_string(row_index(row), &hit_label(&hit, now));
					list.set_selection(row_index(row), true);
					live_region::announce(live, if now { "selected" } else { "not selected" });
				}
				return;
			}
			event.skip(true);
		});
	}
	{
		let app = app.clone();
		add.on_click(move |_| {
			let m = Manager::REAL[mgr.get_selection().unwrap_or(0) as usize];
			let mut added = 0;
			let mut duplicates: Vec<String> = Vec::new();
			{
				let mut st = app.state.borrow_mut();
				for (i, h) in hits.borrow().iter().enumerate() {
					if !picked.borrow().contains(&i) {
						continue;
					}
					if st.config.contains(m, &h.id) {
						duplicates.push(h.name.clone());
					} else {
						st.config.packages.push(Package {
							manager: m,
							name: h.name.clone(),
							id: h.id.clone(),
							args: String::new(),
						});
						added += 1;
					}
				}
			}
			if added > 0 {
				save_config(&app);
				refresh_tree(&app);
			}
			status.set_label(&format!("Added {added} package(s) to toolsy.toml."));
			if !duplicates.is_empty() {
				let msg = if duplicates.len() == 1 {
					format!("{} is already in your config.", duplicates[0])
				} else {
					format!(
						"These packages are already in your config:
{}",
						duplicates.join(
							"
"
						)
					)
				};
				info(&dlg, &msg, "Already added");
			}
		});
	}
	close.on_click(move |_| dlg.end_modal(ID_CANCEL));
	dlg.set_escape_id(ID_CANCEL);

	query.set_focus();
	dlg.show_modal();
	app.state.borrow_mut().search_cb = None;
	dlg.destroy();
}

// ---------------------------------------------------------------- actions

fn add_manual(app: &Rc<App>) {
	let m = app.state.borrow().current;
	if m == Manager::Custom {
		return add_custom(app);
	}
	if let Some(p) = package_dialog(app, "Add package manually", None, m) {
		let dup = app.state.borrow().config.contains(p.manager, &p.id);
		if dup {
			info(&app.frame, "That package is already in your config.", "Duplicate");
			return;
		}
		app.state.borrow_mut().config.packages.push(p);
		save_config(app);
		refresh_tree(app);
	}
}

fn add_custom(app: &Rc<App>) {
	let Some(p) = custom_dialog(app, "Add custom package", None) else { return };
	if app.state.borrow().config.contains(Manager::Custom, &p.id) {
		info(&app.frame, "That URL is already in your config.", "Duplicate");
		return;
	}
	{
		let mut st = app.state.borrow_mut();
		st.config.packages.push(p);
		st.current = Manager::Custom;
	}
	save_config(app);
	refresh_tree(app);
}

fn edit_selected(app: &Rc<App>) {
	let Some(idx) = selected(app).0 else {
		info(&app.frame, "Select a package to edit.", "Edit");
		return;
	};
	let current = app.state.borrow().config.packages[idx].clone();
	let edited = if current.manager == Manager::Custom {
		custom_dialog(app, "Edit custom package", Some(&current))
	} else {
		package_dialog(app, "Edit package", Some(&current), current.manager)
	};
	if let Some(p) = edited {
		{
			let mut st = app.state.borrow_mut();
			st.checked.remove(&(current.manager, current.id));
			st.config.packages[idx] = p;
		}
		save_config(app);
		refresh_tree(app);
	}
}

fn delete_selected(app: &Rc<App>) {
	let Some(idx) = selected(app).0 else {
		info(&app.frame, "Select a package to delete.", "Delete");
		return;
	};
	let p = app.state.borrow().config.packages[idx].clone();
	let msg = format!("Remove \"{}\" ({}) from the config?\nThis does not uninstall it.", p.name, p.id);
	if confirm(&app.frame, &msg, "Delete package") {
		{
			let mut st = app.state.borrow_mut();
			st.checked.remove(&(p.manager, p.id));
			st.config.packages.remove(idx);
		}
		save_config(app);
		refresh_tree(app);
	}
}

fn upgrade_current(app: &Rc<App>) {
	let m = app.state.borrow().current;
	upgrade_all(app, m);
}

fn settings_dialog(app: &Rc<App>) {
	let dlg = Dialog::builder(&app.frame, "Settings").with_size(560, 220).build();
	let sort = CheckBox::builder(&dlg).with_label("&Sort packages alphabetically").build();
	sort.set_value(app.state.borrow().config.sort_alphabetically);
	let extract_label = StaticText::builder(&dlg).with_label("Default &extract location for custom packages:").build();
	let extract = TextCtrl::builder(&dlg).with_value(&app.state.borrow().config.extract_dir).build();
	let browse = Button::builder(&dlg).with_label("&Browse...").build();
	let extract_row = BoxSizer::builder(Orientation::Horizontal).build();
	extract_row.add(&extract, 1, SizerFlag::Expand | SizerFlag::Right, 8);
	extract_row.add(&browse, 0, SizerFlag::AlignCenterVertical, 0);
	let ok = Button::builder(&dlg).with_label("OK").build();
	let cancel = Button::builder(&dlg).with_label("Cancel").build();
	let buttons = BoxSizer::builder(Orientation::Horizontal).build();
	buttons.add_stretch_spacer(1);
	buttons.add(&ok, 0, SizerFlag::All, 4);
	buttons.add(&cancel, 0, SizerFlag::All, 4);
	let top = BoxSizer::builder(Orientation::Vertical).build();
	top.add(&sort, 0, SizerFlag::All, 12);
	top.add(&extract_label, 0, SizerFlag::Left | SizerFlag::Right | SizerFlag::Top, 12);
	top.add_sizer(&extract_row, 0, SizerFlag::Expand | SizerFlag::All, 12);
	top.add_stretch_spacer(1);
	top.add_sizer(&buttons, 0, SizerFlag::Expand | SizerFlag::All, 8);
	dlg.set_sizer(top, true);
	browse.on_click(move |_| {
		let dir = DirDialog::builder(&dlg, "Choose the default extract location", &extract.get_value()).build();
		if dir.show_modal() == ID_OK
			&& let Some(path) = dir.get_path()
		{
			extract.set_value(&path);
		}
	});
	ok.on_click(move |_| dlg.end_modal(ID_OK));
	cancel.on_click(move |_| dlg.end_modal(ID_CANCEL));
	dlg.set_affirmative_id(ID_OK);
	dlg.set_escape_id(ID_CANCEL);
	ok.set_default();
	sort.set_focus();
	if dlg.show_modal() == ID_OK {
		{
			let mut st = app.state.borrow_mut();
			st.config.sort_alphabetically = sort.get_value();
			let dir = extract.get_value().trim().to_string();
			st.config.extract_dir = if dir.is_empty() { config::default_extract_dir() } else { dir };
		}
		save_config(app);
		refresh_packages(app);
	}
	dlg.destroy();
}

fn build_menu_bar(frame: &Frame) {
	let file = Menu::builder().build();
	file.append(ID_SETTINGS, "S&ettings...	Ctrl+,", "Change Toolsy settings", ItemKind::Normal);
	file.append_separator();
	file.append(ID_EXIT, "E&xit", "Close Toolsy", ItemKind::Normal);
	let packages = Menu::builder().build();
	packages.append(
		ID_SEARCH,
		"&Search and Add...	Ctrl+F",
		"Search a package manager and add results to the config",
		ItemKind::Normal,
	);
	packages.append(ID_MANUAL, "Add &Manually...	Ctrl+N", "Add a package by its exact identifier", ItemKind::Normal);
	packages.append(
		ID_CUSTOM,
		"Add &Custom Package...	Ctrl+Shift+N",
		"Add a download URL for an installer or archive",
		ItemKind::Normal,
	);
	packages.append_separator();
	packages.append(ID_INSTALL_SELECTED, "&Install Selected	Ctrl+I", "Install the selected packages", ItemKind::Normal);
	packages.append(ID_UPGRADE_SELECTED, "&Upgrade Selected	Ctrl+U", "Upgrade the selected packages", ItemKind::Normal);
	packages.append(
		ID_UPGRADE_ALL,
		"Upgrade &All	F5",
		"Upgrade everything installed with the current package manager",
		ItemKind::Normal,
	);
	packages.append_separator();
	packages.append(ID_EDIT, "&Edit...	F2", "Edit the highlighted package", ItemKind::Normal);
	packages.append(ID_DELETE, "&Delete	Delete", "Remove the highlighted package from the config", ItemKind::Normal);
	frame.set_menu_bar(MenuBar::builder().append(file, "&File").append(packages, "&Packages").build());
}

fn highlighted(app: &App) -> Option<Package> {
	selected(app).0.map(|i| app.state.borrow().config.packages[i].clone())
}

fn append_install_manager(app: &App, menu: &Menu) {
	let m = app.state.borrow().current;
	if !is_installed(app, m) {
		menu.append(ID_INSTALL_MANAGER, &format!("Install &{}", m.label()), "", ItemKind::Normal);
		menu.append_separator();
	}
}

fn show_package_menu(app: &Rc<App>) {
	let mut menu = Menu::builder().build();
	append_install_manager(app, &menu);
	if highlighted(app).is_some() {
		menu.append(ID_TOGGLE, "Select or &deselect	Space", "", ItemKind::Normal);
		menu.append(ID_INSTALL_ONE, "&Install", "", ItemKind::Normal);
		menu.append(ID_UPGRADE_ONE, "&Upgrade", "", ItemKind::Normal);
		menu.append(ID_EDIT, "&Edit...", "", ItemKind::Normal);
		menu.append(ID_DELETE, "&Delete", "", ItemKind::Normal);
		menu.append_separator();
	}
	menu.append(ID_SEARCH, "&Search and Add...", "", ItemKind::Normal);
	menu.append(ID_MANUAL, "Add &Manually...", "", ItemKind::Normal);
	menu.append(ID_CUSTOM, "Add &Custom Package...", "", ItemKind::Normal);
	app.packages.popup_menu(&mut menu, None);
	menu.destroy_menu();
}

fn show_manager_menu(app: &Rc<App>) {
	let mut menu = Menu::builder().build();
	append_install_manager(app, &menu);
	menu.append(ID_UPGRADE_ALL, "Upgrade &All", "", ItemKind::Normal);
	menu.append(ID_SEARCH, "&Search and Add...", "", ItemKind::Normal);
	app.managers_list.popup_menu(&mut menu, None);
	menu.destroy_menu();
}

fn handle_menu(app: &Rc<App>, id: i32) -> bool {
	match id {
		ID_SEARCH => {
			// Custom packages cannot be searched, so offer the first real manager instead.
			let m = app.state.borrow().current;
			search_dialog(app, if m == Manager::Custom { Manager::Winget } else { m });
		}
		ID_MANUAL => add_manual(app),
		ID_CUSTOM => add_custom(app),
		ID_INSTALL_SELECTED => run_packages(app, selected_packages(app), Action::Install),
		ID_UPGRADE_SELECTED => run_packages(app, selected_packages(app), Action::Upgrade),
		ID_UPGRADE_ALL => upgrade_current(app),
		ID_EDIT => edit_selected(app),
		ID_DELETE => delete_selected(app),
		ID_TOGGLE => {
			if let Some(row) = app.packages.get_selection() {
				toggle_package(app, row as usize);
			}
		}
		ID_INSTALL_ONE => run_packages(app, highlighted(app).into_iter().collect(), Action::Install),
		ID_UPGRADE_ONE => run_packages(app, highlighted(app).into_iter().collect(), Action::Upgrade),
		ID_INSTALL_MANAGER => {
			let m = app.state.borrow().current;
			install_manager(app, m, Box::new(|_, _| {}));
		}
		ID_SETTINGS => settings_dialog(app),
		ID_EXIT => app.frame.close(false),
		_ => return false,
	}
	true
}

fn handle_msg(app: &Rc<App>, msg: Msg) {
	match msg {
		Msg::Line(l) => log(app, &l),
		Msg::Done { job, ok } => {
			let cb = {
				let mut st = app.state.borrow_mut();
				st.busy = false;
				st.jobs.remove(&job)
			};
			app.frame.set_status_text("Ready", 0);
			if let Some(cb) = cb {
				cb(app, ok);
			}
		}
		Msg::Search { token, hits } => {
			let st = app.state.borrow();
			if st.search_token == token
				&& let Some(cb) = &st.search_cb
			{
				cb(hits);
			}
		}
	}
}

// ---------------------------------------------------------------- main

#[allow(clippy::too_many_lines)]
fn main() {
	std::panic::set_hook(Box::new(|info| {
		let _ = std::fs::write("toolsy_crash.log", info.to_string());
	}));
	let _ = wxdragon::main(|_| {
		let frame = Frame::builder().with_title("Toolsy").with_size(Size::new(900, 700)).build();
		frame.create_status_bar(1, 0, i32::try_from(ID_ANY).unwrap_or(-1), "status");
		frame.set_status_text("Ready", 0);
		build_menu_bar(&frame);
		let panel = Panel::builder(&frame).build();

		// Each label sits above its list so screen readers use it as the list's name.
		let managers_label = StaticText::builder(&panel).with_label("Package managers").build();
		let managers_list = ListBox::builder(&panel).build();
		let packages_label = StaticText::builder(&panel).with_label("Packages").build();
		let packages = ListBox::builder(&panel).build();
		let log_label = StaticText::builder(&panel).with_label("Output").build();
		let log_ctrl = TextCtrl::builder(&panel).with_style(TextCtrlStyle::MultiLine | TextCtrlStyle::ReadOnly).build();

		// Made after the lists, so no screen reader takes it for one's label.
		let live_region = StaticText::builder(&panel).with_label("").with_size(Size::new(0, 0)).build();
		live_region.show(false);

		let b_install = Button::builder(&panel).with_label("Install Selected").build();
		let b_upgrade = Button::builder(&panel).with_label("Upgrade Selected").build();
		let lists = BoxSizer::builder(Orientation::Horizontal).build();
		macro_rules! column {
			($weight:expr, $label:expr, $list:expr) => {{
				let column = BoxSizer::builder(Orientation::Vertical).build();
				column.add(&$label, 0, SizerFlag::empty(), 0);
				column.add(&$list, 1, SizerFlag::Expand, 0);
				lists.add_sizer(&column, $weight, SizerFlag::Expand | SizerFlag::All, 4);
			}};
		}
		column!(1, managers_label, managers_list);
		column!(2, packages_label, packages);
		let buttons = BoxSizer::builder(Orientation::Horizontal).build();
		for b in [&b_install, &b_upgrade] {
			buttons.add(b, 0, SizerFlag::All, 4);
		}
		let sizer = BoxSizer::builder(Orientation::Vertical).build();
		sizer.add_sizer(&lists, 3, SizerFlag::Expand, 0);
		sizer.add_sizer(&buttons, 0, SizerFlag::Expand | SizerFlag::All, 4);
		sizer.add(&log_label, 0, SizerFlag::Left, 8);
		sizer.add(&log_ctrl, 1, SizerFlag::Expand | SizerFlag::All, 8);
		panel.set_sizer(sizer, true);

		let (tx, rx) = mpsc::channel::<Msg>();
		RX.with(|r| *r.borrow_mut() = Some(rx));
		let config = Config::load();
		let first_run_done = config.first_run_done;
		let app = Rc::new(App {
			frame,
			managers_list,
			packages,
			live_region,
			packages_label,
			log: log_ctrl,
			tx,
			state: RefCell::new(State {
				config,
				installed: managers::detect(),
				current: Manager::Cargo,
				rows: Vec::new(),
				checked: HashSet::new(),
				next_job: 0,
				busy: false,
				jobs: HashMap::new(),
				search_token: 0,
				search_cb: None,
			}),
		});
		APP.with(|a| *a.borrow_mut() = Some(app.clone()));
		refresh_tree(&app);

		macro_rules! wire {
			($btn:expr, $f:expr) => {{
				let app = app.clone();
				$btn.on_click(move |_| $f(&app));
			}};
		}
		wire!(b_install, |a: &Rc<App>| run_packages(a, selected_packages(a), Action::Install));
		wire!(b_upgrade, |a: &Rc<App>| run_packages(a, selected_packages(a), Action::Upgrade));
		{
			let a = app.clone();
			app.managers_list.on_selection_changed(move |event| {
				if let Some(i) = event.get_selection().and_then(|i| usize::try_from(i).ok())
					&& let Some(m) = manager_at(&a, i)
				{
					{
						let Ok(mut st) = a.state.try_borrow_mut() else { return };
						st.current = m;
					}
					a.packages_label.set_label(&format!("{} packages", m.label()));
					refresh_packages(&a);
				}
			});
			let a = app.clone();
			app.managers_list.on_key_down(move |event| {
				if let WindowEventData::Keyboard(ref key) = event
					&& key.shift_down()
					&& let Some(code @ (WXK_UP | WXK_DOWN)) = key.get_key_code()
				{
					move_manager(&a, if code == WXK_UP { -1 } else { 1 });
					return;
				}
				event.skip(true);
			});
			let a = app.clone();
			app.packages.on_key_down(move |event| {
				if let WindowEventData::Keyboard(ref key) = event
					&& key.get_key_code() == Some(WXK_SPACE)
				{
					if let Some(row) = a.packages.get_selection() {
						toggle_package(&a, row as usize);
					}
					return;
				}
				event.skip(true);
			});
			let a = app.clone();
			app.packages.on_item_double_clicked(move |_| edit_selected(&a));
		}

		{
			let a = app.clone();
			app.frame.on_menu_selected(move |event| {
				if !handle_menu(&a, event.get_id()) {
					event.skip(true);
				}
			});
			let a = app.clone();
			app.packages.bind_internal(EventType::CONTEXT_MENU, move |_| show_package_menu(&a));
			let a = app.clone();
			app.managers_list.bind_internal(EventType::CONTEXT_MENU, move |_| show_manager_menu(&a));
		}

		app.frame.show(true);
		app.frame.centre();
		app.managers_list.set_focus();
		if !first_run_done {
			let app = app;
			// Deferred so the window is up before the first question.
			call_after(Box::new(|| {
				if let Some(app) = APP.with(|a| a.borrow().clone()) {
					let missing = Manager::REAL.into_iter().filter(|m| !is_installed(&app, *m)).collect();
					first_run(&app, missing);
				}
			}));
			drop(app);
		}
	});
}
