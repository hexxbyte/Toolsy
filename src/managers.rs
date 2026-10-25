use std::{
	io::Read,
	os::windows::process::CommandExt,
	path::Path,
	process::{Command, Stdio},
	sync::mpsc::Sender,
};

use crate::config::{Manager, Package};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Refreshes PATH from the registry so freshly installed tools are found,
/// and forces UTF-8 output.
const PS_PREFIX: &str = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
$ProgressPreference='SilentlyContinue'; \
$env:Path=[Environment]::GetEnvironmentVariable('Path','Machine')+';'+[Environment]::GetEnvironmentVariable('Path','User'); ";

pub enum Msg {
	Line(String),
	Done { job: u64, ok: bool },
	Search { token: u64, hits: Vec<Hit> },
}

/// Sends a message to the UI thread and wakes its event loop to deliver it.
pub fn emit(tx: &Sender<Msg>, msg: Msg) {
	if tx.send(msg).is_ok() {
		wxdragon::prelude::call_after(Box::new(crate::drain));
		wxdragon::prelude::wake_up_idle();
	}
}

#[derive(Clone, Debug)]
pub struct Hit {
	pub name: String,
	pub id: String,
	pub version: String,
}

/// PowerShell reading its script from stdin (avoids all quoting problems).
fn powershell() -> Command {
	let mut c = Command::new("powershell.exe");
	c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", "-"]);
	c.creation_flags(CREATE_NO_WINDOW);
	c
}

/// Runs a script and collects its stdout (blocking).
fn capture(script: &str) -> String {
	let full = format!("{PS_PREFIX}{script}");
	let out = powershell().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().and_then(
		|mut child| {
			use std::io::Write;
			if let Some(mut si) = child.stdin.take() {
				let _ = si.write_all(full.as_bytes());
			}
			child.wait_with_output()
		},
	);
	match out {
		Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
		Err(_) => String::new(),
	}
}

/// Which managers are currently available.
pub fn detect() -> Vec<Manager> {
	let out = capture(
		"foreach($n in 'winget','scoop','uv','choco','cargo'){ if(Get-Command $n -ErrorAction SilentlyContinue){ $n } }",
	);
	Manager::REAL.into_iter().filter(|m| out.lines().any(|l| l.trim() == m.command())).collect()
}

fn quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "''"))
}

const WINGET_FLAGS: &str = "--accept-package-agreements --accept-source-agreements";

pub fn install_cmd(p: &Package) -> String {
	let id = quote(&p.id);
	match p.manager {
		Manager::Winget => format!("winget install --id {id} --exact {WINGET_FLAGS} {}", p.args),
		Manager::Scoop => format!("scoop install {id} {}", p.args),
		Manager::Uv => format!("uv tool install {id} {}", p.args),
		Manager::Chocolatey => format!("choco install {id} -y {}", p.args),
		Manager::Cargo => format!("cargo install {id} {}", p.args),
		// Custom packages are downloaded and handled by Toolsy itself, not by a command.
		Manager::Custom => String::new(),
	}
}

pub fn upgrade_cmd(p: &Package) -> String {
	let id = quote(&p.id);
	match p.manager {
		Manager::Winget => format!("winget upgrade --id {id} --exact {WINGET_FLAGS} {}", p.args),
		Manager::Scoop => format!("scoop update {id}; scoop cleanup {id}"),
		Manager::Uv => format!("uv tool upgrade {id}"),
		Manager::Chocolatey => format!("choco upgrade {id} -y {}", p.args),
		Manager::Cargo => format!("cargo install {id} {}", p.args),
		Manager::Custom => String::new(),
	}
}

pub fn upgrade_all_cmd(m: Manager) -> String {
	match m {
		Manager::Winget => format!("winget upgrade --all {WINGET_FLAGS}"),
		Manager::Scoop => "scoop update '*'; scoop cleanup '*'".into(),
		Manager::Uv => "uv tool upgrade --all".into(),
		Manager::Chocolatey => "choco upgrade all -y".into(),
		// cargo has no built-in "upgrade all": reinstall each installed crate (a no-op when current).
		Manager::Cargo => {
			r"cargo install --list | ForEach-Object { if($_ -match '^(\S+) v\S+:'){ cargo install $Matches[1] } }"
				.into()
		}
		Manager::Custom => String::new(),
	}
}

/// Script that downloads `url` to `file`; exits non-zero on failure.
pub fn download_script(url: &str, file: &Path) -> String {
	format!(
		"try {{ Invoke-WebRequest -UseBasicParsing -Uri {} -OutFile {} -ErrorAction Stop }} catch {{ Write-Host \"Download failed: $($_.Exception.Message)\"; exit 1 }}",
		quote(url),
		quote(&file.to_string_lossy())
	)
}

/// Script that runs a downloaded `.exe` (`msi == false`) or `.msi` installer and waits for it.
pub fn run_installer_script(file: &Path, msi: bool) -> String {
	let path = file.to_string_lossy();
	let start = if msi {
		format!(
			"Start-Process -FilePath 'msiexec.exe' -ArgumentList '/i', {} -Wait -PassThru",
			quote(&format!("\"{path}\""))
		)
	} else {
		format!("Start-Process -FilePath {} -Wait -PassThru", quote(&path))
	};
	format!(
		"try {{ $p = {start} -ErrorAction Stop }} catch {{ Write-Host \"Could not start the installer: $($_.Exception.Message)\"; exit 1 }}; Write-Host \"Installer exited with code $($p.ExitCode)\"; exit $p.ExitCode"
	)
}

/// Script that installs a package manager itself. `winget_present`/`scoop_present`
/// decide how uv gets installed.
pub fn install_manager_script(m: Manager, winget_present: bool, scoop_present: bool) -> String {
	match m {
        Manager::Scoop => "Set-ExecutionPolicy RemoteSigned -Scope CurrentUser -Force; \
$admin=([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator); \
if($admin){ iex \"& {$(irm get.scoop.sh)} -RunAsAdmin\" } else { irm get.scoop.sh | iex }"
            .into(),
        Manager::Winget => "$admin=([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator); \
if(-not $admin){ Write-Host 'WARNING: winget-install needs administrator rights. Restart Toolsy as administrator if this fails.' }; \
irm https://github.com/asheroto/winget-install/releases/latest/download/winget-install.ps1 | iex"
            .into(),
        Manager::Chocolatey => {
            if winget_present {
                format!("winget install --id Chocolatey.Chocolatey --exact {WINGET_FLAGS}")
            } else {
                "Set-ExecutionPolicy Bypass -Scope Process -Force; [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor 3072; iex ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))"
                    .into()
            }
        }
        Manager::Cargo => {
            if winget_present {
                format!("winget install --id Rustlang.Rustup --exact {WINGET_FLAGS}")
            } else {
                "$arch=if($env:PROCESSOR_ARCHITECTURE -eq 'ARM64'){'aarch64'}else{'x86_64'}; $exe=Join-Path $env:TEMP 'rustup-init.exe'; Invoke-WebRequest -UseBasicParsing \"https://static.rust-lang.org/rustup/dist/$arch-pc-windows-msvc/rustup-init.exe\" -OutFile $exe; & $exe -y"
                    .into()
            }
        }
        Manager::Uv => {
            if winget_present {
                format!("winget install --id astral-sh.uv --exact {WINGET_FLAGS}")
            } else if scoop_present {
                "scoop install uv".into()
            } else {
                "powershell -ExecutionPolicy ByPass -c \"irm https://astral.sh/uv/install.ps1 | iex\""
                    .into()
            }
        }
        Manager::Custom => String::new(),
    }
}

/// Runs `work` on a background thread (it may send `Msg::Line`s), then reports completion.
pub fn run_task(job: u64, tx: Sender<Msg>, work: impl FnOnce(&Sender<Msg>) -> bool + Send + 'static) {
	std::thread::spawn(move || {
		let ok = work(&tx);
		emit(&tx, Msg::Done { job, ok });
	});
}

/// Runs a script in the background, streaming output lines to `tx`.
pub fn run_job(job: u64, script: String, tx: Sender<Msg>) {
	std::thread::spawn(move || {
		let full = format!("{PS_PREFIX}{script}");
		let mut cmd = powershell();
		cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
		let mut child = match cmd.spawn() {
			Ok(c) => c,
			Err(e) => {
				emit(&tx, Msg::Line(format!("Failed to start PowerShell: {e}")));
				emit(&tx, Msg::Done { job, ok: false });
				return;
			}
		};
		{
			use std::io::Write;
			if let Some(mut si) = child.stdin.take() {
				let _ = si.write_all(full.as_bytes());
			}
		}
		let mut readers = Vec::new();
		if let Some(o) = child.stdout.take() {
			readers.push(pump(o, tx.clone()));
		}
		if let Some(e) = child.stderr.take() {
			readers.push(pump(e, tx.clone()));
		}
		let status = child.wait();
		for r in readers {
			let _ = r.join();
		}
		let ok = status.is_ok_and(|s| s.success());
		emit(&tx, Msg::Done { job, ok });
	});
}

/// Splits a stream into lines on `\n` and `\r` (progress bars use `\r`).
fn pump<R: Read + Send + 'static>(mut r: R, tx: Sender<Msg>) -> std::thread::JoinHandle<()> {
	std::thread::spawn(move || {
		let mut buf = [0u8; 4096];
		let mut line: Vec<u8> = Vec::new();
		let flush = |line: &mut Vec<u8>, tx: &Sender<Msg>| {
			let s = String::from_utf8_lossy(line).trim_end().to_string();
			line.clear();
			let t = s.trim();
			if !t.is_empty() && !matches!(t, "-" | "\\" | "|" | "/") {
				emit(tx, Msg::Line(s));
			}
		};
		loop {
			match r.read(&mut buf) {
				Ok(0) | Err(_) => break,
				Ok(n) => {
					for &b in &buf[..n] {
						if b == b'\n' || b == b'\r' {
							flush(&mut line, &tx);
						} else {
							line.push(b);
						}
					}
				}
			}
		}
		flush(&mut line, &tx);
	})
}

/// Searches a package manager (blocking; run on a worker thread).
pub fn search(m: Manager, query: &str) -> Vec<Hit> {
	let q = quote(query);
	match m {
		Manager::Winget => parse_winget(&capture(&format!("winget search {q} --accept-source-agreements"))),
		Manager::Scoop => parse_scoop(&capture(&format!(
			"scoop search {q} | ForEach-Object {{ if($_.Name){{ \"$($_.Name)|$($_.Version)|$($_.Source)\" }} else {{ $_ }} }}"
		))),
		Manager::Chocolatey => parse_pipe(&capture(&format!("choco search {q} --limit-output"))),
		Manager::Cargo => capture(&format!("cargo search {q} --limit 25"))
			.lines()
			.filter_map(|l| {
				let (n, rest) = l.split_once(" = \"")?;
				let v = rest.split('"').next()?;
				Some(Hit { name: n.trim().into(), id: n.trim().into(), version: v.into() })
			})
			.collect(),
		// uv has no search; treat the query as an exact PyPI name and verify it.
		Manager::Custom => Vec::new(),
		Manager::Uv => {
			let out = capture(&format!(
				"try {{ $i=(Invoke-RestMethod -UseBasicParsing ('https://pypi.org/pypi/'+[uri]::EscapeDataString({q})+'/json')).info; \"$($i.name)|$($i.version)\" }} catch {{}}"
			));
			out.lines()
				.filter_map(|l| {
					let (n, v) = l.trim().split_once('|')?;
					Some(Hit { name: n.into(), id: n.into(), version: v.into() })
				})
				.collect()
		}
	}
}

fn parse_winget(text: &str) -> Vec<Hit> {
	let lines: Vec<&str> = text.lines().collect();
	let Some(h) = lines.iter().position(|l| l.contains("Name") && l.contains(" Id") && l.contains("Version")) else {
		return Vec::new();
	};
	let header: Vec<char> = lines[h].chars().collect();
	let find = |word: &str| -> Option<usize> {
		let w: Vec<char> = word.chars().collect();
		(0..header.len().saturating_sub(w.len() - 1)).find(|&i| header[i..i + w.len()] == w[..])
	};
	let (Some(id_at), Some(ver_at)) = (find("Id"), find("Version")) else {
		return Vec::new();
	};
	let end_at = find("Match").or_else(|| find("Source"));
	lines[h + 1..]
		.iter()
		.filter(|l| !l.starts_with("---") && !l.trim().is_empty())
		.filter_map(|l| {
			let c: Vec<char> = l.chars().collect();
			if c.len() <= ver_at {
				return None;
			}
			let slice = |a: usize, b: usize| -> String {
				c[a.min(c.len())..b.min(c.len())].iter().collect::<String>().trim().to_string()
			};
			let name = slice(0, id_at);
			let id = slice(id_at, ver_at);
			let version = slice(ver_at, end_at.unwrap_or(c.len()));
			(!id.is_empty()).then_some(Hit { name, id, version })
		})
		.collect()
}

/// Parses `id|version` lines.
fn parse_pipe(text: &str) -> Vec<Hit> {
	text.lines()
		.filter_map(|l| {
			let (id, v) = l.trim().split_once('|')?;
			(!id.is_empty()).then(|| Hit { name: id.into(), id: id.into(), version: v.into() })
		})
		.collect()
}

fn parse_scoop(text: &str) -> Vec<Hit> {
	text.lines()
		.filter_map(|l| {
			let parts: Vec<&str> = l.trim().split('|').collect();
			if parts.len() < 3 || parts[0].is_empty() {
				return None;
			}
			let (name, version, bucket) = (parts[0], parts[1], parts[2]);
			let id = if bucket.is_empty() || bucket == "main" { name.to_string() } else { format!("{bucket}/{name}") };
			Some(Hit { name: name.into(), id, version: version.into() })
		})
		.collect()
}
