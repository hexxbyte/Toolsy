use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
	Winget,
	Scoop,
	Uv,
	Chocolatey,
	Cargo,
	/// A direct download URL (installer or archive), not a real package manager.
	Custom,
}

impl Manager {
	pub const ALL: [Self; 6] = [Self::Cargo, Self::Chocolatey, Self::Scoop, Self::Uv, Self::Winget, Self::Custom];
	/// The managers that are separate programs (everything except `Custom`).
	pub const REAL: [Self; 5] = [Self::Cargo, Self::Chocolatey, Self::Scoop, Self::Uv, Self::Winget];

	pub const fn label(self) -> &'static str {
		match self {
			Self::Winget => "Winget",
			Self::Scoop => "Scoop",
			Self::Uv => "uv",
			Self::Chocolatey => "Chocolatey",
			Self::Cargo => "Cargo",
			Self::Custom => "Custom",
		}
	}

	/// Name of the executable / command.
	pub const fn command(self) -> &'static str {
		match self {
			Self::Winget => "winget",
			Self::Scoop => "scoop",
			Self::Uv => "uv",
			Self::Chocolatey => "choco",
			Self::Cargo => "cargo",
			Self::Custom => "",
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Package {
	pub manager: Manager,
	pub name: String,
	pub id: String,
	/// Extra parameters passed to the package manager, e.g. `--architecture x64`.
	#[serde(default, skip_serializing_if = "String::is_empty")]
	pub args: String,
}

#[derive(Serialize, Deserialize)]
pub struct Config {
	#[serde(default)]
	pub first_run_done: bool,
	/// Show packages sorted alphabetically instead of in config order.
	#[serde(default)]
	pub sort_alphabetically: bool,
	/// Folder custom packages are extracted to; each gets its own subfolder.
	#[serde(default = "default_extract_dir")]
	pub extract_dir: String,
	/// User-chosen order of the manager list; managers not listed follow alphabetically.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub manager_order: Vec<Manager>,
	#[serde(default, rename = "package")]
	pub packages: Vec<Package>,
}

pub fn default_extract_dir() -> String {
	std::env::current_dir().unwrap_or_default().join("custom").to_string_lossy().into_owned()
}

impl Default for Config {
	fn default() -> Self {
		Self {
			first_run_done: false,
			sort_alphabetically: false,
			extract_dir: default_extract_dir(),
			manager_order: Vec::new(),
			packages: Vec::new(),
		}
	}
}

pub fn config_path() -> PathBuf {
	PathBuf::from("toolsy.toml")
}

impl Config {
	pub fn load() -> Self {
		fs::read_to_string(config_path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
	}

	pub fn save(&self) -> Result<(), String> {
		let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
		fs::write(config_path(), text).map_err(|e| e.to_string())
	}

	/// The managers in display order: the saved order first, then any others in default order.
	pub fn managers(&self) -> Vec<Manager> {
		let mut out: Vec<Manager> = Vec::with_capacity(Manager::ALL.len());
		for m in self.manager_order.iter().copied().chain(Manager::ALL) {
			if !out.contains(&m) {
				out.push(m);
			}
		}
		out
	}

	pub fn contains(&self, manager: Manager, id: &str) -> bool {
		self.packages.iter().any(|p| p.manager == manager && p.id.eq_ignore_ascii_case(id))
	}
}
