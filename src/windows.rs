use crate::config::{Browser, Configuration};
use anyhow::{bail, Context, Result};
use const_format::concatcp;
use log::{debug, error, info, trace, warn};
use simplelog::*;
use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use structopt::StructOpt;
use winreg::{enums::*, RegKey};

// How many bytes do we let the log size grow to before we rotate it? We only keep one current and one old log.
const MAX_LOG_SIZE: u64 = 64 * 1024;

const CANONICAL_NAME: &str = "bichrome.exe";
const PROGID: &str = "bichromeHTML";

// Configuration for "Default Programs". StartMenuInternet is the key for browsers
// and they're expected to use the name of the exe as the key.
const DPROG_PATH: &str = concatcp!(r"SOFTWARE\Clients\StartMenuInternet\", CANONICAL_NAME);
const DPROG_INSTALLINFO_PATH: &str = concatcp!(DPROG_PATH, "InstallInfo");

const APPREG_BASE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\";
const PROGID_PATH: &str = concatcp!(r"SOFTWARE\Classes\", PROGID);
const REGISTERED_APPLICATIONS_PATH: &str =
    concatcp!(r"SOFTWARE\RegisteredApplications\", DISPLAY_NAME);

const DISPLAY_NAME: &str = "bichrome";
const DESCRIPTION: &str = "Pick the right Chrome profile for each URL";

/// Retrieve an EXE path by looking in the registry for the App Paths entry
fn get_exe_path(exe_name: &str) -> Result<PathBuf> {
    for root_name in &[HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let root = RegKey::predef(*root_name);
        if let Ok(subkey) = root.open_subkey(format!("{}{}", APPREG_BASE, exe_name)) {
            if let Ok(value) = subkey.get_value::<String, _>("") {
                let path = PathBuf::from(value);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }

    bail!("Could not find path for {}", exe_name);
}

/// Register associations with Windows for being a browser
fn register_urlhandler(extra_args: Option<&str>) -> io::Result<()> {
    // This is used both by initial registration and OS-invoked reinstallation.
    // The expectations for the latter are documented here: https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#the-reinstall-command
    use std::env::current_exe;

    let exe_path = current_exe()?;
    let exe_name = exe_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();

    let exe_path = exe_path.to_str().unwrap_or_default().to_owned();
    let icon_path = format!("\"{}\",0", exe_path);
    let open_command = if let Some(extra_args) = extra_args {
        format!("\"{}\" {} \"%1\"", exe_path, extra_args)
    } else {
        format!("\"{}\" \"%1\"", exe_path)
    };

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    // Configure our ProgID to point to the right command
    {
        let (progid_class, _) = hkcu.create_subkey(PROGID_PATH)?;
        progid_class.set_value("", &DISPLAY_NAME)?;

        let (progid_class_defaulticon, _) = progid_class.create_subkey("DefaultIcon")?;
        progid_class_defaulticon.set_value("", &icon_path)?;

        let (progid_class_shell_open_command, _) =
            progid_class.create_subkey(r"shell\open\command")?;
        progid_class_shell_open_command.set_value("", &open_command)?;
    }

    // Set up the Default Programs configuration for the app (https://docs.microsoft.com/en-us/windows/win32/shell/default-programs)
    {
        let (dprog, _) = hkcu.create_subkey(DPROG_PATH)?;
        dprog.set_value("", &DISPLAY_NAME)?;
        dprog.set_value("LocalizedString", &DISPLAY_NAME)?;

        let (dprog_capabilites, _) = dprog.create_subkey("Capabilities")?;
        dprog_capabilites.set_value("ApplicationName", &DISPLAY_NAME)?;
        dprog_capabilites.set_value("ApplicationIcon", &icon_path)?;
        dprog_capabilites.set_value("ApplicationDescription", &DESCRIPTION)?;

        let (dprog_capabilities_startmenu, _) = dprog_capabilites.create_subkey("Startmenu")?;
        dprog_capabilities_startmenu.set_value("StartMenuInternet", &CANONICAL_NAME)?;

        // Register for various URL protocols that our target browsers might support.
        // (The list of protocols that Chrome registers for is actually quite large, including irc, mailto, mms,
        // etc, but let's do the most obvious/significant ones.)
        let (dprog_capabilities_urlassociations, _) =
            dprog_capabilites.create_subkey("URLAssociations")?;
        for protocol in &["bichrome", "ftp", "http", "https", "webcal"] {
            dprog_capabilities_urlassociations.set_value(protocol, &PROGID)?;
        }

        // Register for various file types, so that we'll be invoked for file:// URLs for these types (e.g.
        // by `cargo doc --open`.)
        let (dprog_capabilities_fileassociations, _) =
            dprog_capabilites.create_subkey("FileAssociations")?;
        for filetype in &[
            ".htm", ".html", ".pdf", ".shtml", ".svg", ".webp", ".xht", ".xhtml",
        ] {
            dprog_capabilities_fileassociations.set_value(filetype, &PROGID)?;
        }

        let (dprog_defaulticon, _) = dprog.create_subkey("DefaultIcon")?;
        dprog_defaulticon.set_value("", &icon_path)?;

        // Set up reinstallation and show/hide icon commands (https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#registering-installation-information)
        let (dprog_installinfo, _) = dprog.create_subkey("InstallInfo")?;
        dprog_installinfo.set_value("ReinstallCommand", &format!("\"{}\" register", exe_path))?;
        dprog_installinfo.set_value("HideIconsCommand", &format!("\"{}\" hide-icons", exe_path))?;
        dprog_installinfo.set_value("ShowIconsCommand", &format!("\"{}\" show-icons", exe_path))?;

        // Only update IconsVisible if it hasn't been set already
        if dprog_installinfo
            .get_value::<u32, _>("IconsVisible")
            .is_err()
        {
            dprog_installinfo.set_value("IconsVisible", &1u32)?;
        }

        let (dprog_shell_open_command, _) = dprog.create_subkey(r"shell\open\command")?;
        dprog_shell_open_command.set_value("", &open_command)?;
    }

    // Set up a registered application for our Default Programs capabilities (https://docs.microsoft.com/en-us/windows/win32/shell/default-programs#registeredapplications)
    {
        let (registered_applications, _) =
            hkcu.create_subkey(r"SOFTWARE\RegisteredApplications")?;
        let dprog_capabilities_path = format!(r"{}\Capabilities", DPROG_PATH);
        registered_applications.set_value(DISPLAY_NAME, &dprog_capabilities_path)?;
    }

    // Application Registration (https://docs.microsoft.com/en-us/windows/win32/shell/app-registration)
    {
        let appreg_path = format!(r"{}{}", APPREG_BASE, exe_name);
        let (appreg, _) = hkcu.create_subkey(appreg_path)?;
        // This is used to resolve "bichrome.exe" -> full path, if needed.
        appreg.set_value("", &exe_path)?;
        // UseUrl indicates that we don't need the shell to download a file for us -- we can handle direct
        // HTTP URLs.
        appreg.set_value("UseUrl", &1u32)?;
    }

    refresh_shell();

    Ok(())
}

fn refresh_shell() {
    use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_DWORD, SHCNF_FLUSH};

    // Notify the shell about the updated URL associations. (https://docs.microsoft.com/en-us/windows/win32/shell/default-programs#becoming-the-default-browser)
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_DWORD | SHCNF_FLUSH, None, None);
    }
}

/// Remove all the registry keys that we've set up
fn unregister_urlhandler() {
    use std::env::current_exe;

    // Find the current executable's name, so we can unregister it
    let exe_name = current_exe()
        .unwrap()
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned();

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let _ = hkcu.delete_subkey_all(DPROG_PATH);
    let _ = hkcu.delete_subkey_all(PROGID_PATH);
    let _ = hkcu.delete_subkey(REGISTERED_APPLICATIONS_PATH);
    let _ = hkcu.delete_subkey_all(format!("{}{}", APPREG_BASE, exe_name));
    refresh_shell();
}

/// Set the "IconsVisible" flag to true (we don't have any icons)
fn show_icons() -> io::Result<()> {
    // The expectations for this are documented here: https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#the-show-icons-command
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (dprog_installinfo, _) = hkcu.create_subkey(DPROG_INSTALLINFO_PATH)?;
    dprog_installinfo.set_value("IconsVisible", &1u32)
}

/// Set the "IconsVisible" flag to false (we don't have any icons)
fn hide_icons() -> io::Result<()> {
    // The expectations for this are documented here: https://docs.microsoft.com/en-us/windows/win32/shell/reg-middleware-apps#the-hide-icons-command
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(dprog_installinfo) = hkcu.open_subkey(DPROG_INSTALLINFO_PATH) {
        dprog_installinfo.set_value("IconsVisible", &0u32)
    } else {
        Ok(())
    }
}

fn get_local_app_data_path() -> Option<PathBuf> {
    use windows::Storage::UserDataPaths;
    if let Ok(user_data_paths) = UserDataPaths::GetDefault() {
        if let Ok(local_app_data_path) = user_data_paths.LocalAppData() {
            return Some(PathBuf::from(local_app_data_path.to_string()));
        }
    }

    None
}

/// Find the path to Chrome's "Local State" in the user's local app data folder
pub fn get_chrome_local_state_path() -> Option<PathBuf> {
    let app_data_relative = r"Google\Chrome\User Data\Local State";
    get_local_app_data_path().map(|base| base.join(app_data_relative))
}

// This is the definition of our command line options
#[derive(Debug, StructOpt)]
#[structopt(
    name = "bichrome",
    about = "A program to pick Chrome profile based on the URL opened"
)]
struct CommandOptions {
    /// Use verbose logging
    #[structopt(short, long)]
    verbose: bool,
    /// Use debug logging, even more verbose than --verbose
    #[structopt(long)]
    debug: bool,

    /// Do not launch Chrome, just log what would've been launched
    #[structopt(long)]
    dry_run: bool,

    /// Choose the mode of operation
    #[structopt(subcommand)]
    mode: Option<ExecutionMode>,

    /// List of URLs to open
    urls: Vec<String>,
}

#[derive(Debug, Clone, Copy, StructOpt)]
enum ExecutionMode {
    /// Open the given URLs in the correct browser
    Open,
    /// Register bichrome as a valid browser
    Register,
    /// Remove previous registration of bichrome, if any
    Unregister,
    /// Show application icons (changes a registry key and nothing else, as we don't have icons)
    ShowIcons,
    /// Hide application icons (changes a registry key and nothing else, as we don't have icons)
    HideIcons,
}

fn get_exe_relative_path(filename: &str) -> io::Result<PathBuf> {
    let mut path = std::env::current_exe()?;
    path.set_file_name(filename);
    Ok(path)
}

fn rotate_and_open_log(log_path: &Path) -> Result<File, io::Error> {
    if let Ok(log_info) = std::fs::metadata(log_path) {
        if log_info.len() > MAX_LOG_SIZE
            && std::fs::rename(log_path, log_path.with_extension("log.old")).is_err()
            && std::fs::remove_file(log_path).is_err()
        {
            return File::create(log_path);
        }
    }

    return OpenOptions::new().append(true).create(true).open(log_path);
}

fn init() -> Result<CommandOptions> {
    // First parse our command line options, so we can use it to configure the logging.
    let options = CommandOptions::from_args();
    let log_level = if options.debug {
        LevelFilter::Trace
    } else if options.verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    let log_path = get_exe_relative_path("bichrome.log")?;
    // Always log to bichrome.log
    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![WriteLogger::new(
        log_level,
        Config::default(),
        rotate_and_open_log(&log_path)?,
    )];
    // We only use the terminal logger in the debug build, since we don't allocate a console window otherwise.
    if cfg!(debug_assertions) {
        loggers.push(TermLogger::new(
            log_level,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ));
    };

    CombinedLogger::init(loggers)?;
    trace!("command line options: {:?}", options);

    Ok(options)
}

fn read_config() -> io::Result<Configuration> {
    let config_path = get_exe_relative_path("bichrome_config.json")?;
    // We try to read the config, and otherwise just use an empty one instead.
    debug!("attempting to load config from {}", config_path.display());
    let config = Configuration::read_from_file(&config_path);
    Ok(match config {
        Ok(config) => {
            trace!("config: {:#?}", config);
            config
        }
        Err(e) => {
            error!("failed to parse config: {:?}", e);
            warn!("opening URLs without profile");
            Configuration::empty()
        }
    })
}

pub fn main() -> Result<()> {
    let options = init()?;

    let mode = options.mode.unwrap_or(if options.urls.is_empty() {
        ExecutionMode::Register
    } else {
        ExecutionMode::Open
    });

    if !matches!(mode, ExecutionMode::Open) && !options.urls.is_empty() {
        bail!(
            "Specified a list of URLs for mode {:?} which doesn't take URLs",
            mode
        );
    }

    match mode {
        ExecutionMode::Register => {
            if options.dry_run {
                info!("(dry-run) would register URL handler")
            } else {
                info!("registering URL handler");
                let extra_args = if options.debug {
                    Some("--debug")
                } else if options.verbose {
                    Some("--verbose")
                } else {
                    None
                };

                register_urlhandler(extra_args).context("Failed to register URL handler")?;
            }
        }
        ExecutionMode::Unregister => {
            if options.dry_run {
                info!("(dry-run) would unregister URL handler")
            } else {
                info!("unregistering URL handler");
                unregister_urlhandler();
            }
        }
        ExecutionMode::ShowIcons => {
            if options.dry_run {
                info!("(dry-run) would mark icons as visible")
            } else {
                info!("marking icons as visible");
                show_icons().context("Failed to show icons")?;
            }
        }
        ExecutionMode::HideIcons => {
            if options.dry_run {
                info!("(dry-run) would mark icons as hidden")
            } else {
                info!("marking icons as hidden");

                hide_icons().context("Failed to hide icons")?;
            }
        }
        ExecutionMode::Open => {
            let config = read_config()?;

            for url in options.urls {
                let browser = config.choose_browser(&url)?;
                let (exe, args) = match &browser {
                    Browser::Chrome(profile) => {
                        let mut args = Vec::new();
                        if let Some(argument) = profile.get_argument()? {
                            args.push(argument);
                        }
                        args.push(url.to_string());

                        (get_exe_path("chrome.exe")?, args)
                    }
                    Browser::Firefox => (get_exe_path("firefox.exe")?, vec![url.to_string()]),
                    Browser::OsDefault => (get_exe_path("msedge.exe")?, vec![url.to_string()]),
                    Browser::Edge(profile) => {
                        let mut args = Vec::new();
                        if let Some(argument) = profile.get_argument()? {
                            args.push(argument);
                        }
                        args.push(url.to_string());

                        (get_exe_path("msedge.exe")?, args)
                    }
                    Browser::Safari => {
                        bail!("Apple Safari not supported on Windows")
                    }
                };

                let commandline = format!("\"{}\" \"{}\"", exe.display(), args.join("\" \""));
                if options.dry_run {
                    info!("(dry-run) {}", commandline);
                } else {
                    // Allow any process to steal focus from us, so that we will transfer focus "nicely" to
                    // the browser.
                    use windows::Win32::UI::WindowsAndMessaging::{
                        AllowSetForegroundWindow, ASFW_ANY,
                    };
                    unsafe {
                        if let Err(error) = AllowSetForegroundWindow(ASFW_ANY) {
                            warn!("Could not `AllowSetForegroundWindow`: {error:?}");
                        }
                    }

                    // Let's not log the URL to the logs by default, so there's not a gross log file
                    // the user might not be aware of inadvertently 'tracking' their browsing activity.
                    info!("picked {:?}", &browser);
                    debug!("launching {}", commandline);
                    Command::new(&exe)
                        .stdout(Stdio::null())
                        .stdin(Stdio::null())
                        .stderr(Stdio::null())
                        .args(args)
                        .spawn()
                        .with_context(|| {
                            format!(
                                "Failed to launch browser {:?} for URL {}, attempted command {}",
                                &browser, url, commandline
                            )
                        })?;
                }
            }
        }
    }

    Ok(())
}
