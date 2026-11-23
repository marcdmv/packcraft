//! macOS Finder right-click actions ("Quick Actions"). Each is an Automator
//! service bundle in ~/Library/Services that runs one shell line: start
//! PackCraft with the selected files and a launch flag. Installing writes
//! the bundles and asks the services database to rescan; removing deletes
//! only the bundles PackCraft wrote (they all start with "PackCraft").

use std::path::{Path, PathBuf};

/// (menu title, launch flag). The bundle name is the menu title.
pub const ACTIONS: [(&str, &str); 4] = [
    ("PackCraft - Compress", "--add-default"),
    ("PackCraft - Compress with settings", "--add"),
    ("PackCraft - Extract here", "--extract-here"),
    ("PackCraft - Extract to folder", "--extract-to"),
];

fn services_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Services"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Single-quoted for /bin/sh.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shell line for one action. Inside an app bundle it goes through
/// `open` so macOS treats it as an app launch (Dock icon, focus) and
/// Automator returns at once; a bare binary is started in the background.
pub fn command(exe: &Path, flag: &str) -> String {
    let exe_s = exe.to_string_lossy();
    match exe_s.find(".app/Contents/MacOS/") {
        Some(i) => format!("open -n -a {} --args {flag} \"$@\"", sh_quote(&exe_s[..i + 4])),
        None => format!("nohup {} {flag} \"$@\" >/dev/null 2>&1 &", sh_quote(&exe_s)),
    }
}

fn info_plist(title: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSBackgroundColorName</key>
			<string>background</string>
			<key>NSIconName</key>
			<string>NSActionTemplate</string>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>{}</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict>
				<key>NSApplicationIdentifier</key>
				<string>com.apple.finder</string>
			</dict>
			<key>NSSendFileTypes</key>
			<array>
				<string>public.item</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
"#,
        xml(title)
    )
}

fn document_wflow(cmd: &str, n: usize) -> String {
    // Fixed ids per action keep reinstalls byte-identical.
    let uuid = |k: usize| format!("5C0F1A2B-3D4E-4F50-8A6B-{:012X}", n * 16 + k);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key>
	<string>523</string>
	<key>AMApplicationVersion</key>
	<string>2.10</string>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>AMAccepts</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Optional</key>
					<true/>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>AMActionVersion</key>
				<string>2.0.3</string>
				<key>AMApplication</key>
				<array>
					<string>Automator</string>
				</array>
				<key>AMParameterProperties</key>
				<dict>
					<key>COMMAND_STRING</key>
					<dict/>
					<key>CheckedForUserDefaultShell</key>
					<dict/>
					<key>inputMethod</key>
					<dict/>
					<key>shell</key>
					<dict/>
					<key>source</key>
					<dict/>
				</dict>
				<key>AMProvides</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>ActionBundlePath</key>
				<string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key>
				<string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>1</integer>
					<key>shell</key>
					<string>/bin/sh</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key>
				<string>2.0.3</string>
				<key>CanShowSelectedItemsWhenRun</key>
				<false/>
				<key>CanShowWhenRun</key>
				<true/>
				<key>Category</key>
				<array>
					<string>AMCategoryUtilities</string>
				</array>
				<key>Class Name</key>
				<string>RunShellScriptAction</string>
				<key>InputUUID</key>
				<string>{}</string>
				<key>Keywords</key>
				<array>
					<string>Shell</string>
					<string>Script</string>
				</array>
				<key>OutputUUID</key>
				<string>{}</string>
				<key>UUID</key>
				<string>{}</string>
				<key>UnlocalizedApplications</key>
				<array>
					<string>Automator</string>
				</array>
				<key>arguments</key>
				<dict/>
				<key>isViewVisible</key>
				<integer>1</integer>
				<key>location</key>
				<string>309.000000:253.000000</string>
				<key>nibPath</key>
				<string>/System/Library/Automator/Run Shell Script.action/Contents/Resources/Base.lproj/main.nib</string>
			</dict>
			<key>isViewVisible</key>
			<integer>1</integer>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>applicationBundleIDsByPath</key>
		<dict/>
		<key>applicationPaths</key>
		<array/>
		<key>inputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>outputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>presentationMode</key>
		<integer>15</integer>
		<key>processesInput</key>
		<integer>0</integer>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<integer>0</integer>
		<key>systemImageName</key>
		<string>NSActionTemplate</string>
		<key>useAutomaticInputType</key>
		<integer>0</integer>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#,
        xml(cmd),
        uuid(1),
        uuid(2),
        uuid(3)
    )
}

fn bundle(dir: &Path, title: &str) -> PathBuf {
    dir.join(format!("{title}.workflow"))
}

/// Writes the four Quick Actions for the running binary. Returns where.
pub fn install() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the PackCraft program: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    install_into(&services_dir()?, &exe)?;
    refresh();
    services_dir()
}

pub fn install_into(dir: &Path, exe: &Path) -> Result<(), String> {
    for (n, (title, flag)) in ACTIONS.iter().enumerate() {
        let contents = bundle(dir, title).join("Contents");
        std::fs::create_dir_all(&contents).map_err(|e| format!("{}: {e}", contents.display()))?;
        let w = |name: &str, text: String| std::fs::write(contents.join(name), text).map_err(|e| format!("{}: {e}", contents.join(name).display()));
        w("Info.plist", info_plist(title))?;
        w("document.wflow", document_wflow(&command(exe, flag), n))?;
    }
    Ok(())
}

/// Deletes the Quick Actions PackCraft installed; returns how many.
pub fn remove() -> Result<usize, String> {
    let n = remove_from(&services_dir()?)?;
    refresh();
    Ok(n)
}

pub fn remove_from(dir: &Path) -> Result<usize, String> {
    let mut n = 0;
    for (title, _) in ACTIONS {
        let b = bundle(dir, title);
        // Only a directory that holds our own two files; never anything else.
        if b.join("Contents/document.wflow").is_file() {
            std::fs::remove_dir_all(&b).map_err(|e| format!("{}: {e}", b.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

pub fn installed() -> bool {
    services_dir().is_ok_and(|d| ACTIONS.iter().all(|(t, _)| bundle(&d, t).join("Contents/document.wflow").is_file()))
}

/// Asks macOS to rescan services so the menu updates without logging out.
fn refresh() {
    let _ = std::process::Command::new("/System/Library/CoreServices/pbs").arg("-update").status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_quote_paths_and_use_open_for_bundles() {
        let c = command(Path::new("/Applications/Pack Craft.app/Contents/MacOS/packcraft-gui"), "--add");
        assert_eq!(c, "open -n -a '/Applications/Pack Craft.app' --args --add \"$@\"");
        let c = command(Path::new("/opt/it's/packcraft-gui"), "--extract-here");
        assert_eq!(c, "nohup '/opt/it'\\''s/packcraft-gui' --extract-here \"$@\" >/dev/null 2>&1 &");
    }

    #[test]
    fn install_then_remove_touches_only_our_bundles() {
        let d = std::env::temp_dir().join(format!("packcraft-services-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("Other.workflow/Contents")).unwrap();
        install_into(&d, Path::new("/Applications/PackCraft.app/Contents/MacOS/packcraft-gui")).unwrap();
        for (t, flag) in ACTIONS {
            let wf = std::fs::read_to_string(bundle(&d, t).join("Contents/document.wflow")).unwrap();
            assert!(wf.contains(&format!("--args {flag} &quot;$@&quot;")), "{t}");
            let info = std::fs::read_to_string(bundle(&d, t).join("Contents/Info.plist")).unwrap();
            assert!(info.contains(t));
        }
        assert_eq!(remove_from(&d).unwrap(), 4);
        assert!(d.join("Other.workflow/Contents").is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }
}
