use embed_manifest::{
	embed_manifest,
	manifest::{ActiveCodePage, DpiAwareness, SupportedOS::Windows10},
	new_manifest,
};

fn main() {
	if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
		embed_manifest(
			new_manifest("Toolsy")
				.supported_os(Windows10..=Windows10)
				.active_code_page(ActiveCodePage::Utf8)
				.dpi_awareness(DpiAwareness::PerMonitorV2),
		)
		.expect("unable to embed manifest");
	}
	println!("cargo:rerun-if-changed=build.rs");
}
