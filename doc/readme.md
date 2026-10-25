# Toolsy User Manual

Toolsy is a Windows app for keeping a list of the programs you install and installing or upgrading them in one go.

It supports five package managers, plus custom download URLs:

* [winget](https://learn.microsoft.com/windows/package-manager/winget/)
* [Scoop](https://scoop.sh)
* [uv](https://docs.astral.sh/uv/) - installed as tools with `uv tool install`
* [Chocolatey](https://chocolatey.org) - uses `choco`; installing packages requires administrator rights
* [Cargo](https://doc.rust-lang.org/cargo/) - installs crates with `cargo install`

## Using Toolsy

The main window contains two lists:

* Managers - the list on the left contains the available package managers.
* Packages - the list on the right contains the packages you've added for the manager selected on the left.

### Search and Add - Ctrl+F

Select a package manager, search for a package, and press Space on the packages you want.

Then choose Add selected to config.

This only saves the packages to `toolsy.toml`; it does not install them.

### Add Manually - Ctrl+N

Enter the exact package identifier and, optionally, any parameters.

### Add Custom Package - Ctrl+Shift+N

Enter a package name and a download URL.

See [Custom packages](#custom-packages) for details.

### Edit - F2

Edit the exact package identifier and its parameters.

The package list displays only the package name. Use Edit when you need to view or change the package identifier or parameters.

### Delete - Delete

Removes a package from the configuration after confirmation.

Deleting a package does not uninstall the program.

### Select - Space

Marks a package for installation or upgrade.

Selections are preserved when you switch between package managers.

### Install Selected - Ctrl+I

Installs all currently selected packages.

### Upgrade Selected - Ctrl+U

Upgrades all currently selected packages.

### Upgrade All - F5

Upgrades everything installed through the currently selected package manager.

The command used depends on the manager:

* winget: `winget upgrade --all`
* Scoop: `scoop update *` followed by `scoop cleanup *`
* uv: `uv tool upgrade --all`
* Chocolatey: `choco upgrade all -y`
* Cargo: Reinstalls every crate listed by `cargo install --list`, since Cargo does not provide an upgrade-all command.

### Settings - Ctrl+Comma

You can also open Settings from the File menu.

Available settings include:

* Sort packages alphabetically - off by default. This changes only how packages are displayed; it does not change their order in `toolsy.toml`.
* Default extract location - specifies where custom archive packages are extracted. A Browse button is provided.

You can open the context menu for either list by pressing Shift+F10, or pressing the Applications key.

Command output appears in the Output box at the bottom of the window.

## Reordering the Managers

The managers list is alphabetical by default, with Custom always last.

To reorder the managers:

1. Select a manager in the managers list.
2. Press Shift+Up or Shift+Down to move it.

The custom order is saved in `toolsy.toml`.

To restore the default order, delete the `manager_order` line from `toolsy.toml`.

## Custom Packages

The Custom entry in the managers list contains packages that consist of a download URL (`http` or `https`).

When you install a custom package, Toolsy:

1. Downloads the file.
2. Determines its type from its contents rather than from the URL.
3. Asks for confirmation before taking any further action.

### EXE and MSI files

Toolsy asks:

> Run it now?

An MSI file is launched with:

```
msiexec /i
```

### Archive files

Toolsy supports:

* `.zip`
* `.7z`
* `.tar`
* `.gz`
* `.bz2`
* `.xz`
* `.zst`
* Combined formats such as `.tar.gz`

It asks:

> Extract it to ...?

Files are extracted into a subfolder named after the package inside the default extract location configured in Settings.

By default, the extract location is the `custom` folder next to the folder where you started Toolsy. You can change this location in Settings.

Existing files are overwritten.

For security, Toolsy refuses archive entries that would extract outside the destination folder.

RAR archives are not supported.

### Other file types

For any other file type, Toolsy offers to save the downloaded file in that package's folder.

### Upgrading Custom Packages

Upgrade Selected downloads the file again and asks for confirmation before proceeding.

The Custom manager does not support Search or Upgrade All.

## Package Managers

On its first run, Toolsy checks which supported package managers are installed.

If a manager is missing, Toolsy offers to install it. If you decline, Toolsy will ask again the next time you try to use that manager.

### Scoop

Scoop is installed using the script from `get.scoop.sh`.

### winget

winget is installed using [asheroto/winget-install](https://github.com/asheroto/winget-install).

Installing winget requires administrator rights, so run Toolsy as an administrator when performing this step.

### uv

Toolsy installs uv using the following order:

1. winget, if available
2. Scoop, if available
3. The standalone installer from `astral.sh`

### Chocolatey

Toolsy installs Chocolatey using:

1. winget (`Chocolatey.Chocolatey`), if winget is available
2. The installation script from `community.chocolatey.org`

Installing Chocolatey requires administrator rights, so run Toolsy as an administrator when performing this step.

### Cargo

Toolsy installs Cargo using:

1. winget (`Rustlang.Rustup`), if winget is available
2. `rustup-init.exe`, downloaded for your PC's architecture (x86_64 or aarch64) and run with `-y`

Installing Rust requires administrator rights, so run Toolsy as an administrator when performing this step.

### Searching for uv Packages

Searching for uv packages works differently from the other package managers because uv does not provide a search command.

The text you enter is treated as an exact PyPI package name, and Toolsy checks that package against PyPI.

## `toolsy.toml`

Toolsy saves your package list in `toolsy.toml` in the folder from which you start the app.