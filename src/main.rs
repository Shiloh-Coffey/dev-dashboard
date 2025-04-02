#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use sysinfo::{System, SystemExt, CpuExt, DiskExt, NetworkExt, NetworksExt, ProcessExt};
use std::time::{Duration, Instant};
use std::collections::HashMap;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExA;
use windows::core::PCSTR;
use wmi::{COMLibrary, WMIConnection};
use nvml_wrapper::Nvml;
use log::{error, info, warn, debug};
use simplelog::{WriteLogger, LevelFilter, Config};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use whoami::username;
use serde::{Serialize, Deserialize};
use futures::StreamExt;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::error::Error as StdError;
use winreg::enums::*;
use winreg::RegKey;
use std::ffi::CString;
use reqwest::Client;
use glob::glob;
use tokio::process::Command as TokioCommand;
use egui::RichText;

#[derive(Debug)]
enum InstallerError {
    NoAppsSelected,
    DownloadFailed(String),
    ChannelError(String),
}

// Implement Send for InstallerError
unsafe impl Send for InstallerError {}

impl std::fmt::Display for InstallerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallerError::NoAppsSelected => write!(f, "No apps selected"),
            InstallerError::DownloadFailed(msg) => write!(f, "Download failed: {}", msg),
            InstallerError::ChannelError(msg) => write!(f, "Communication error: {}", msg),
        }
    }
}

impl StdError for InstallerError {}

// Define our custom Result type
type InstallerResult<T> = std::result::Result<T, Box<dyn StdError + Send + 'static>>;

impl From<InstallerError> for Box<dyn StdError + Send + 'static> {
    fn from(error: InstallerError) -> Self {
        Box::new(error)
    }
}

impl From<std::io::Error> for InstallerError {
    fn from(error: std::io::Error) -> Self {
        InstallerError::DownloadFailed(error.to_string())
    }
}

impl From<reqwest::Error> for InstallerError {
    fn from(error: reqwest::Error) -> Self {
        InstallerError::DownloadFailed(error.to_string())
    }
}

// Create newtypes for external error types
struct SendErrorWrapper(std::sync::mpsc::SendError<InstallerMessage>);
struct ReqwestErrorWrapper(reqwest::Error);
struct IoErrorWrapper(std::io::Error);

impl From<SendErrorWrapper> for Box<dyn StdError + Send + 'static> {
    fn from(error: SendErrorWrapper) -> Self {
        Box::new(InstallerError::ChannelError(error.0.to_string()))
    }
}

impl From<ReqwestErrorWrapper> for Box<dyn StdError + Send + 'static> {
    fn from(error: ReqwestErrorWrapper) -> Self {
        Box::new(InstallerError::DownloadFailed(error.0.to_string()))
    }
}

impl From<IoErrorWrapper> for Box<dyn StdError + Send + 'static> {
    fn from(error: IoErrorWrapper) -> Self {
        Box::new(InstallerError::DownloadFailed(error.0.to_string()))
    }
}

// Add conversions from original error types to our wrappers
impl From<std::sync::mpsc::SendError<InstallerMessage>> for SendErrorWrapper {
    fn from(error: std::sync::mpsc::SendError<InstallerMessage>) -> Self {
        SendErrorWrapper(error)
    }
}

impl From<reqwest::Error> for ReqwestErrorWrapper {
    fn from(error: reqwest::Error) -> Self {
        ReqwestErrorWrapper(error)
    }
}

impl From<std::io::Error> for IoErrorWrapper {
    fn from(error: std::io::Error) -> Self {
        IoErrorWrapper(error)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    custom_username: Option<String>,
}

#[derive(PartialEq)]
enum Tab {
    Dashboard,
    Tools,
}

#[derive(Clone)]
enum InstallerMessage {
    UpdateProgress(f32),
    SetState(InstallerState),
    Error(String),
}

#[derive(PartialEq, Clone)]
enum InstallerState {
    Idle,
    Downloading,
    Installing,
    Error(String),
}

/// Linear interpolation function for smooth value transitions
/// start: Starting value
/// end: Target value
/// t: Interpolation factor (0.0 to 1.0)
fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t.clamp(0.0, 1.0)
}

/// Structure to track network interface statistics
/// Handles both cumulative and real-time network usage data
struct NetworkStats {
    total_received: u64,      // Total bytes received since start
    total_sent: u64,          // Total bytes sent since start
    last_received: u64,       // Last recorded received bytes
    last_sent: u64,           // Last recorded sent bytes
    received_speed: f64,      // Current receive speed in bytes/second
    sent_speed: f64,          // Current send speed in bytes/second
    last_update: Instant,     // Timestamp of last update
}

/// Structure for smooth value transitions with animation
/// Used for updating UI elements with smooth transitions
struct AnimatedValue {
    current: f32,    // Current displayed value
    target: f32,     // Target value to animate towards
}

impl AnimatedValue {
    /// Creates a new animated value starting at the specified value
    fn new(value: f32) -> Self {
        Self {
            current: value,
            target: value,
        }
    }

    /// Updates the current value towards the target using exponential smoothing
    /// delta_time: Time elapsed since last update in seconds
    fn update(&mut self, delta_time: f32) {
        let smoothing = 1.0 - (-8.0 * delta_time).exp();
        self.current = lerp(self.current, self.target, smoothing);
    }

    /// Sets a new target value to animate towards
    fn set_target(&mut self, target: f32) {
        self.target = target;
    }
}

/// Structure to hold GPU information and statistics
/// Supports both NVIDIA GPUs (via NVML) and other GPUs (via WMI)
struct GpuInfo {
    name: String,                    // GPU model name
    memory_total: Option<u64>,       // Total VRAM in bytes
    memory_used: Option<u64>,        // Used VRAM in bytes
    utilization: Option<f32>,        // GPU utilization percentage
    temperature: Option<u32>,        // GPU temperature in Celsius
    memory_usage: AnimatedValue,     // Animated VRAM usage percentage
    gpu_usage: AnimatedValue,        // Animated GPU utilization percentage
    pci_bus_id: Option<String>,      // PCI bus ID for hardware identification
    driver_version: Option<String>,  // GPU driver version
}

impl GpuInfo {
    /// Creates a new GPU info structure with default values
    fn new(name: String) -> Self {
        Self {
            name,
            memory_total: None,
            memory_used: None,
            utilization: None,
            temperature: None,
            memory_usage: AnimatedValue::new(0.0),
            gpu_usage: AnimatedValue::new(0.0),
            pci_bus_id: None,
            driver_version: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct NiniteApp {
    name: String,
    category: String,
    ninite_id: String,
    registry_keys: Vec<String>,  // Registry keys to check for installation
    file_paths: Vec<String>,     // Common installation file paths to check
    installed: bool,
}

impl NiniteApp {
    fn new(name: &str, category: &str, ninite_id: &str, registry_keys: Vec<&str>, file_paths: Vec<&str>) -> Self {
        Self {
            name: name.to_string(),
            category: category.to_string(),
            ninite_id: ninite_id.to_string(),
            registry_keys: registry_keys.iter().map(|&s| s.to_string()).collect(),
            file_paths: file_paths.iter().map(|&s| s.to_string()).collect(),
            installed: false,
        }
    }

    fn check_installation(&mut self) {
        debug!("Checking installation for {}", self.name);
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        
        // Check both 64-bit and 32-bit registry views
        let views = [KEY_READ | KEY_WOW64_64KEY, KEY_READ | KEY_WOW64_32KEY];
        
        // Check registry keys
        let registry_installed = self.registry_keys.iter().any(|key_path| {
            let result = views.iter().any(|view| {
                // Try HKLM first
                if hklm.open_subkey_with_flags(key_path, *view).is_ok() {
                    info!("Found {} in HKLM registry: {}", self.name, key_path);
                    return true;
                }
                // Then try HKCU
                if hkcu.open_subkey_with_flags(key_path, *view).is_ok() {
                    info!("Found {} in HKCU registry: {}", self.name, key_path);
                    return true;
                }
                debug!("Registry key not found for {}: {} (view: {})", 
                    self.name, key_path, 
                    if view & KEY_WOW64_64KEY != 0 { "64-bit" } else { "32-bit" });
                false
            });
            result
        });

        // Get current username for path expansion
        let username = whoami::username();
        debug!("Checking file paths for {} with username {}", self.name, username);

        // Check file paths with environment variable expansion and glob support
        let file_installed = self.file_paths.iter().any(|path| {
            // Replace %USERNAME% with actual username
            let expanded_path = path.replace("%USERNAME%", &username);
            debug!("Checking path for {}: {}", self.name, expanded_path);
            
            // Use glob for wildcard pattern matching
            if expanded_path.contains('*') {
                match glob(&expanded_path) {
                    Ok(entries) => {
                        let found = entries.filter_map(Result::ok).any(|path| {
                            debug!("Checking glob match for {}: {:?}", self.name, path);
                            // Verify the file exists and is executable
                            if let Ok(metadata) = std::fs::metadata(&path) {
                                if metadata.is_file() {
                                    info!("Found {} at path: {:?}", self.name, path);
                                    return true;
                                }
                                debug!("Path exists but is not a file: {:?}", path);
                            }
                            false
                        });
                        if !found {
                            debug!("No valid executables found for glob pattern: {}", expanded_path);
                        }
                        found
                    }
                    Err(e) => {
                        warn!("Failed to check glob pattern for {}: {}", self.name, e);
                        false
                    }
                }
            } else {
                let path = std::path::Path::new(&expanded_path);
                let exists = path.exists() && path.is_file();
                if exists {
                    info!("Found {} at path: {}", self.name, expanded_path);
                } else {
                    debug!("Path not found or not a file for {}: {}", self.name, expanded_path);
                }
                exists
            }
        });

        // Additional registry checks for uninstall entries
        let uninstall_installed = {
            let uninstall_key = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
            let uninstall_key_wow64 = "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
            
            let check_uninstall = |key_path: &str| -> bool {
                if let Ok(uninstall) = hklm.open_subkey_with_flags(key_path, KEY_READ) {
                    if let Ok(subkeys) = uninstall.enum_keys().collect::<Result<Vec<_>, _>>() {
                        for subkey in subkeys {
                            if let Ok(app_key) = uninstall.open_subkey(&subkey) {
                                if let Ok(display_name) = app_key.get_value::<String, _>("DisplayName") {
                                    let display_name_lower = display_name.to_lowercase();
                                    let app_name_lower = self.name.to_lowercase();
                                    if display_name_lower.contains(&app_name_lower) {
                                        // Also check InstallLocation if available
                                        if let Ok(install_location) = app_key.get_value::<String, _>("InstallLocation") {
                                            let location_path = std::path::Path::new(&install_location);
                                            if !location_path.exists() || !location_path.is_dir() {
                                                debug!("Install location doesn't exist for {}: {}", self.name, install_location);
                                                return false;
                                            }
                                        }
                                        info!("Found {} in uninstall registry: {}", self.name, display_name);
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
                false
            };
            
            check_uninstall(uninstall_key) || check_uninstall(uninstall_key_wow64)
        };

        let was_installed = self.installed;
        
        // Mark as installed if the executable is found, regardless of registry keys
        self.installed = file_installed;
        
        if self.installed != was_installed {
            info!("Installation status changed for {}: {} -> {}", self.name, was_installed, self.installed);
            if !self.installed {
                debug!("Detection failed - Registry: {}, File: {}, Uninstall: {}", 
                    registry_installed, file_installed, uninstall_installed);
            }
        }
    }
}

/// Main application structure that holds all system monitoring data
/// Manages system information collection and UI rendering
pub struct DevDashboard {
    sys: System,                     // System information instance
    last_update: Instant,            // Last system info update timestamp
    last_network_update: Instant,    // Last network stats update timestamp
    last_frame_time: Instant,        // Last UI frame timestamp
    last_check: Instant,             // Last Ninite check timestamp
    current_cpu_usage: AnimatedValue, // Animated CPU usage percentage
    memory_usage: AnimatedValue,     // Animated memory usage percentage
    disk_usage: HashMap<String, AnimatedValue>, // Disk usage per drive
    network_stats: HashMap<String, NetworkStats>, // Network stats per interface
    gpu_info: Option<GpuInfo>,       // GPU information if available
    nvml: Option<Nvml>,              // NVIDIA Management Library instance
    settings: Settings,              // Application settings
    show_settings: bool,             // Whether to show settings window
    current_tab: Tab,                // Current selected tab
    ninite_apps: Vec<NiniteApp>,     // List of available Ninite apps
    selected_apps: Vec<String>,      // Selected apps for installation
    download_progress: f32,          // Download progress (0.0 to 1.0)
    installer_state: InstallerState, // Current state of the installer
    runtime: Option<tokio::runtime::Runtime>, // Tokio runtime for async operations
    message_receiver: Option<Receiver<InstallerMessage>>,
    ninite_running: bool,
}

impl Default for DevDashboard {
    fn default() -> Self {
        let mut sys = System::new_all();
        let mut network_stats = HashMap::new();
        
        sys.refresh_all();
        sys.refresh_disks();
        
        for (name, data) in sys.networks() {
            if DevDashboard::is_physical_interface(name) {
                network_stats.insert(name.to_string(), NetworkStats {
                    total_received: data.received(),
                    total_sent: data.transmitted(),
                    last_received: data.received(),
                    last_sent: data.transmitted(),
                    received_speed: 0.0,
                    sent_speed: 0.0,
                    last_update: Instant::now(),
                });
            }
        }

        let mut disk_usage = HashMap::new();
        let disks = sys.disks();
        
        for disk in disks {
            let mount_point = disk.mount_point().to_string_lossy().to_string();
            
            if mount_point.len() == 2 && mount_point.ends_with(":") {
                let total_space = disk.total_space();
                let available_space = disk.available_space();
                
                if total_space > 0 {
                    let usage = (total_space - available_space) as f64 / total_space as f64;
                    disk_usage.insert(mount_point, AnimatedValue::new(usage as f32));
                } else {
                    warn!("Disk {} has zero total space", mount_point);
                }
            }
        }

        let gpu_info = Self::initialize_gpu();
        let nvml = Nvml::init().ok();

        // Load settings from file
        let settings = Self::load_settings();

        // Initialize Ninite apps with registry keys and file paths
        let ninite_apps = vec![
            NiniteApp::new("Chrome", "Web Browsers", "chrome", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\chrome.exe",
                "SOFTWARE\\Google\\Chrome"
            ], vec![
                "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
                "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Google\\Chrome\\Application\\chrome.exe"
            ]),
            NiniteApp::new("Firefox", "Web Browsers", "firefox", vec![
                "SOFTWARE\\Mozilla\\Mozilla Firefox",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\firefox.exe"
            ], vec![
                "C:\\Program Files\\Mozilla Firefox\\firefox.exe",
                "C:\\Program Files (x86)\\Mozilla Firefox\\firefox.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Mozilla Firefox\\firefox.exe"
            ]),
            NiniteApp::new("Edge", "Web Browsers", "edge", vec![
                "SOFTWARE\\Microsoft\\Edge",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\msedge.exe"
            ], vec![
                "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
                "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"
            ]),
            NiniteApp::new("Zoom", "Messaging", "zoom", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ZoomUMX",
                "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\ZoomUMX",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Zoom"
            ], vec![
                "C:\\Program Files\\Zoom\\bin\\Zoom.exe",
                "C:\\Program Files (x86)\\Zoom\\bin\\Zoom.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Roaming\\Zoom\\bin\\Zoom.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Zoom\\bin\\Zoom.exe"
            ]),
            NiniteApp::new("Discord", "Messaging", "discord", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\Discord",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\Discord.exe"
            ], vec![
                "C:\\Program Files\\Discord\\Discord.exe",
                "C:\\Program Files (x86)\\Discord\\Discord.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Discord\\app-*\\Discord.exe"
            ]),
            NiniteApp::new("VLC", "Media", "vlc", vec![
                "SOFTWARE\\VideoLAN\\VLC",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\vlc.exe"
            ], vec![
                "C:\\Program Files\\VideoLAN\\VLC\\vlc.exe",
                "C:\\Program Files (x86)\\VideoLAN\\VLC\\vlc.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\VideoLAN\\VLC\\vlc.exe"
            ]),
            NiniteApp::new("Audacity", "Media", "audacity", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\audacity.exe"
            ], vec![
                "C:\\Program Files\\Audacity\\audacity.exe",
                "C:\\Program Files (x86)\\Audacity\\audacity.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\Audacity\\audacity.exe"
            ]),
            NiniteApp::new("Blender", "Imaging", "blender", vec![
                "SOFTWARE\\BlenderFoundation",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\blender.exe"
            ], vec![
                "C:\\Program Files\\Blender Foundation\\Blender *\\blender.exe",
                "C:\\Program Files (x86)\\Blender Foundation\\Blender *\\blender.exe"
            ]),
            NiniteApp::new("Paint.NET", "Imaging", "paintdotnet", vec![
                "SOFTWARE\\Paint.NET"
            ], vec![
                "C:\\Program Files\\paint.net\\PaintDotNet.exe",
                "C:\\Program Files (x86)\\paint.net\\PaintDotNet.exe"
            ]),
            NiniteApp::new("GIMP", "Imaging", "gimp", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\gimp-2.10.exe",
                "SOFTWARE\\Classes\\GIMP-2.10",
                "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\GIMP-2.10"
            ], vec![
                "C:\\Program Files\\GIMP 3\\bin\\gimp.exe",
                "C:\\Program Files (x86)\\GIMP 3\\bin\\gimp.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\GIMP 3\\bin\\gimp.exe"
            ]),
            NiniteApp::new("LibreOffice", "Documents", "libreoffice", vec![
                "SOFTWARE\\LibreOffice",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\soffice.exe"
            ], vec![
                "C:\\Program Files\\LibreOffice\\program\\soffice.exe",
                "C:\\Program Files (x86)\\LibreOffice\\program\\soffice.exe"
            ]),
            NiniteApp::new("Python", "Developer Tools", "python", vec![
                "SOFTWARE\\Python\\PythonCore"
            ], vec![
                "C:\\Program Files\\Python*\\python.exe",
                "C:\\Program Files (x86)\\Python*\\python.exe",
                "C:\\Python*\\python.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\Python\\Python*\\python.exe"
            ]),
            NiniteApp::new("FileZilla", "Developer Tools", "filezilla", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\FileZilla Client"
            ], vec![
                "C:\\Program Files\\FileZilla FTP Client\\filezilla.exe",
                "C:\\Program Files (x86)\\FileZilla FTP Client\\filezilla.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\FileZilla FTP Client\\filezilla.exe"
            ]),
            NiniteApp::new("Notepad++", "Developer Tools", "notepadplusplus", vec![
                "SOFTWARE\\Notepad++"
            ], vec![
                "C:\\Program Files\\Notepad++\\notepad++.exe",
                "C:\\Program Files (x86)\\Notepad++\\notepad++.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\Notepad++\\notepad++.exe"
            ]),
            NiniteApp::new("WinSCP", "Developer Tools", "winscp", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\winscp3_is1",
                "SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\winscp3_is1",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\WinSCP.exe"
            ], vec![
                "C:\\Program Files\\WinSCP\\WinSCP.exe",
                "C:\\Program Files (x86)\\WinSCP\\WinSCP.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\WinSCP\\WinSCP.exe"
            ]),
            NiniteApp::new("PuTTY", "Developer Tools", "putty", vec![
                "SOFTWARE\\SimonTatham\\PuTTY"
            ], vec![
                "C:\\Program Files\\PuTTY\\putty.exe",
                "C:\\Program Files (x86)\\PuTTY\\putty.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\PuTTY\\putty.exe"
            ]),
            NiniteApp::new("Visual Studio Code", "Developer Tools", "vscode", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{771FD6B0-FA20-440A-A002-3B3BAC16DC50}_is1",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\VSCode",
                "SOFTWARE\\Classes\\Applications\\Code.exe"
            ], vec![
                "C:\\Program Files\\Microsoft VS Code\\Code.exe",
                "C:\\Program Files (x86)\\Microsoft VS Code\\Code.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe"
            ]),
            NiniteApp::new("Evernote", "Other", "evernote", vec![
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\Evernote.exe"
            ], vec![
                "C:\\Program Files\\Evernote\\Evernote.exe",
                "C:\\Program Files (x86)\\Evernote\\Evernote.exe"
            ]),
            NiniteApp::new("Google Earth", "Other", "googleearth", vec![
                "SOFTWARE\\Google\\Google Earth Pro",
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\googleearth.exe"
            ], vec![
                "C:\\Program Files\\Google\\Google Earth Pro\\client\\googleearth.exe",
                "C:\\Program Files (x86)\\Google\\Google Earth Pro\\client\\googleearth.exe"
            ]),
            NiniteApp::new("7-Zip", "Compression", "7zip", vec![
                "SOFTWARE\\7-Zip"
            ], vec![
                "C:\\Program Files\\7-Zip\\7z.exe",
                "C:\\Program Files (x86)\\7-Zip\\7z.exe"
            ]),
            NiniteApp::new("WinRAR", "Compression", "winrar", vec![
                "SOFTWARE\\WinRAR"
            ], vec![
                "C:\\Program Files\\WinRAR\\WinRAR.exe",
                "C:\\Program Files (x86)\\WinRAR\\WinRAR.exe"
            ]),
            NiniteApp::new("qBittorrent", "File Sharing", "qbittorrent", vec![
                "SOFTWARE\\qBittorrent"
            ], vec![
                "C:\\Program Files\\qBittorrent\\qbittorrent.exe",
                "C:\\Program Files (x86)\\qBittorrent\\qbittorrent.exe",
                "C:\\Users\\%USERNAME%\\AppData\\Local\\Programs\\qBittorrent\\qbittorrent.exe"
            ]),
        ];

        Self {
            sys,
            last_update: Instant::now(),
            last_network_update: Instant::now(),
            last_frame_time: Instant::now(),
            last_check: Instant::now(),
            current_cpu_usage: AnimatedValue::new(0.0),
            memory_usage: AnimatedValue::new(0.0),
            disk_usage,
            network_stats,
            gpu_info,
            nvml,
            settings,
            show_settings: false,
            current_tab: Tab::Dashboard,
            ninite_apps,
            selected_apps: Vec::new(),
            download_progress: 0.0,
            installer_state: InstallerState::Idle,
            runtime: None,
            message_receiver: None,
            ninite_running: false,
        }
    }
}

impl DevDashboard {
    fn load_settings() -> Settings {
        match File::open("settings.json") {
            Ok(mut file) => {
                let mut contents = String::new();
                if file.read_to_string(&mut contents).is_ok() {
                    if let Ok(settings) = serde_json::from_str(&contents) {
                        return settings;
                    }
                }
                Settings::default()
            }
            Err(_) => Settings::default(),
        }
    }

    fn save_settings(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.settings) {
            if let Ok(mut file) = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open("settings.json") {
                let _ = file.write_all(json.as_bytes());
            }
        }
    }

    fn show_settings_window(&mut self, ctx: &egui::Context) {
        if self.show_settings {
            egui::Window::new("Settings")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.vertical(|ui| {
                        ui.heading("Customize Your Dashboard");
                        ui.add_space(8.0);
                        
                        ui.label("Display Name:");
                        let mut username = self.settings.custom_username.clone().unwrap_or_default();
                        if ui.text_edit_singleline(&mut username).changed() {
                            self.settings.custom_username = if username.is_empty() {
                                None
                            } else {
                                Some(username)
                            };
                            self.save_settings();
                        }
                        
                        ui.add_space(16.0);
                        
                        if ui.button("Close").clicked() {
                            self.show_settings = false;
                        }
                    });
                });
        }
    }

    fn show_tools_tab(&mut self, ui: &mut egui::Ui) {
        // Refresh installed apps status periodically
        static mut LAST_CHECK: Option<Instant> = None;
        
        let should_refresh = unsafe {
            if let Some(last) = LAST_CHECK {
                if last.elapsed() >= Duration::from_secs(2) {
                    LAST_CHECK = Some(Instant::now());
                    true
                } else {
                    false
                }
            } else {
                LAST_CHECK = Some(Instant::now());
                true
            }
        };

        if should_refresh {
            info!("Refreshing program installation status...");
            for app in &mut self.ninite_apps {
                app.check_installation();
            }
        }

        // Process any pending messages
        if let Some(receiver) = &self.message_receiver {
            while let Ok(message) = receiver.try_recv() {
                match message {
                    InstallerMessage::UpdateProgress(progress) => {
                        self.download_progress = progress;
                    }
                    InstallerMessage::SetState(new_state) => {
                        let should_refresh = new_state == InstallerState::Idle;
                        self.installer_state = new_state;
                        if should_refresh {
                            info!("Installation completed, refreshing program status...");
                            // Update installation status
                            for app in &mut self.ninite_apps {
                                app.check_installation();
                                if app.installed {
                                    self.selected_apps.retain(|name| name != &app.name);
                                }
                            }
                        }
                    }
                    InstallerMessage::Error(error) => {
                        error!("Installer error: {}", error);
                        self.installer_state = InstallerState::Error(error);
                    }
                }
            }
        }

        // Create a frame that will contain all the tools content
        egui::Frame::none()
            .inner_margin(egui::style::Margin::same(10.0))
            .show(ui, |ui| {
                ui.set_enabled(self.installer_state == InstallerState::Idle);

                ui.heading("Essential Tools Installation");
                ui.add_space(8.0);

                // Create a stable ordering of categories
                let categories = [
                    "Web Browsers",
                    "Messaging",
                    "Media",
                    "Imaging",
                    "Documents",
                    "Developer Tools",
                    "Other",
                    "Compression",
                    "File Sharing",
                ];

                // Show apps grouped by category with stable ordering
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for &category in &categories {
                        let apps: Vec<&NiniteApp> = self.ninite_apps.iter()
                            .filter(|app| app.category == category)
                            .collect();

                        if !apps.is_empty() {
                            ui.collapsing(category, |ui| {
                                for app in apps {
                                    let mut is_selected = self.selected_apps.contains(&app.name);
                                    
                                    ui.horizontal(|ui| {
                                        if app.installed {
                                            ui.add_enabled(false, egui::Checkbox::new(&mut false, &app.name));
                                            ui.label(" (Installed)");
                                        } else {
                                            if ui.checkbox(&mut is_selected, &app.name).changed() {
                                                if is_selected {
                                                    debug!("Selected app for installation: {}", app.name);
                                                    self.selected_apps.push(app.name.clone());
                                                } else {
                                                    debug!("Deselected app: {}", app.name);
                                                    self.selected_apps.retain(|x| x != &app.name);
                                                }
                                            }
                                        }
                                    });
                                }
                            });
                        }
                    }

                    ui.add_space(16.0);

                    match &self.installer_state {
                        InstallerState::Downloading => {
                            ui.add_space(8.0);
                            ui.vertical_centered(|ui| {
                                ui.heading("Downloading Ninite Installer...");
                                ui.add_space(4.0);
                                ui.add(egui::ProgressBar::new(self.download_progress)
                                    .text(format!("{:.0}%", self.download_progress * 100.0)));
                            });
                        }
                        InstallerState::Installing => {
                            ui.add_space(8.0);
                            ui.vertical_centered(|ui| {
                                ui.heading("Installing Selected Applications...");
                                ui.add_space(4.0);
                                ui.label("This may take a few minutes. Please wait for the Ninite installer to complete.");
                                ui.add_space(8.0);
                                // Add an animated spinner
                                let time = ui.input(|i| i.time);
                                let angle = time * std::f64::consts::PI;
                                let (sin, cos) = angle.sin_cos();
                                let points = (0..8).map(|i| {
                                    let angle = i as f64 * std::f64::consts::PI / 4.0;
                                    let (s, c) = angle.sin_cos();
                                    let distance = 20.0 * (1.0 + 0.3 * (sin * c + cos * s)) as f32;
                                    egui::pos2(
                                        ui.available_width() / 2.0 + distance * c as f32,
                                        50.0 + distance * s as f32,
                                    )
                                }).collect::<Vec<_>>();
                                let painter = ui.painter();
                                for (i, point) in points.iter().enumerate() {
                                    let alpha = 1.0 - (i as f32 / points.len() as f32);
                                    painter.circle_filled(
                                        *point,
                                        4.0,
                                        egui::Color32::from_white_alpha((alpha * 255.0) as u8),
                                    );
                                }
                            });
                        }
                        InstallerState::Error(error) => {
                            let error_msg = error.clone();
                            ui.add_space(8.0);
                            ui.vertical_centered(|ui| {
                                ui.colored_label(egui::Color32::from_rgb(220, 50, 50), format!("Error: {}", error_msg));
                                ui.add_space(8.0);
                                let retry = ui.button("Retry").clicked();
                                if retry {
                                    info!("Retrying installation...");
                                    self.installer_state = InstallerState::Idle;
                                }
                            });
                        }
                        InstallerState::Idle => {
                            if !self.selected_apps.is_empty() {
                                ui.vertical_centered(|ui| {
                                    if ui.button("Install Selected Apps").clicked() {
                                        info!("Starting installation of selected apps: {:?}", self.selected_apps);
                                        // Initialize runtime if not already done
                                        if self.runtime.is_none() {
                                            self.runtime = Some(tokio::runtime::Runtime::new().unwrap());
                                        }

                                        // Create a channel for communication
                                        let (sender, receiver) = channel();
                                        self.message_receiver = Some(receiver);

                                        // Clone the necessary data for the async task
                                        let selected_apps = self.selected_apps.clone();
                                        let ninite_apps = self.ninite_apps.clone();

                                        // Start the download process
                                        if let Some(runtime) = &self.runtime {
                                            runtime.spawn(async move {
                                                if let Err(e) = Self::download_ninite_installer(
                                                    selected_apps,
                                                    ninite_apps,
                                                    sender.clone()
                                                ).await {
                                                    error!("Download failed: {}", e);
                                                    sender.send(InstallerMessage::Error(e.to_string())).ok();
                                                }
                                            });
                                        }
                                    }
                                });
                            }
                        }
                    }
                });
        });

        // Show overlay message when installer is running
        if self.installer_state == InstallerState::Installing {
            let screen_rect = ui.ctx().screen_rect();
            let overlay_id = ui.make_persistent_id("installer_overlay");
            egui::Area::new(overlay_id)
                .fixed_pos(screen_rect.center())
                .order(egui::Order::Foreground)
                .show(ui.ctx(), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(20.0);
                        let frame = egui::Frame::none()
                            .fill(egui::Color32::from_black_alpha(240))
                            .rounding(8.0)
                            .inner_margin(egui::style::Margin::same(20.0))
                            .shadow(egui::epaint::Shadow {
                                extrusion: 8.0,
                                color: egui::Color32::from_black_alpha(100),
                            });

                        frame.show(ui, |ui| {
                            ui.heading("Installation in Progress");
                            ui.add_space(8.0);
                            ui.label("The Ninite installer is running.");
                            ui.label("Please complete the installation before making any changes.");
                            ui.add_space(8.0);
                            ui.label("This window will be enabled once the installation is complete.");
                        });
                    });
                });
        }
    }

    fn get_disk_space(path: &str) -> Option<(u64, u64)> {
        let path_cstr = CString::new(path).ok()?;
        let mut total_bytes = 0u64;
        let mut free_bytes = 0u64;
        
        unsafe {
            let result = GetDiskFreeSpaceExA(
                PCSTR(path_cstr.as_ptr() as *const u8),
                Some(&mut free_bytes),
                Some(&mut total_bytes),
                None,
            );
            
            if result.as_bool() {
                Some((total_bytes, free_bytes))
            } else {
                None
            }
        }
    }

    async fn download_ninite_installer(
        selected_apps: Vec<String>,
        ninite_apps: Vec<NiniteApp>,
        sender: Sender<InstallerMessage>
    ) -> InstallerResult<()> {
        if selected_apps.is_empty() {
            return Err(Box::new(InstallerError::NoAppsSelected));
        }

        Self::send_message(&sender, InstallerMessage::SetState(InstallerState::Downloading))?;

        // Create Ninite URL with selected apps
        let app_ids: Vec<String> = selected_apps.iter()
            .filter_map(|name| {
                ninite_apps.iter()
                    .find(|app| app.name == *name)
                    .map(|app| app.ninite_id.clone())
            })
            .collect();

        let joined = app_ids.join("-");
        let url = format!("https://ninite.com/{}/ninite.exe", joined);

        // Download the installer
        let client = Client::new();
        let response = client.get(&url).send().await.map_err(ReqwestErrorWrapper)?;

        if !response.status().is_success() {
            return Err(Box::new(InstallerError::DownloadFailed(
                format!("Server returned: {}", response.status())
            )));
        }

        let total_size = response.content_length().unwrap_or(0);
        let mut downloaded = 0u64;

        // Clean up any existing installer file
        let installer_path = "ninite.exe";
        if std::path::Path::new(installer_path).exists() {
            match std::fs::remove_file(installer_path) {
                Ok(_) => info!("Removed existing installer file"),
                Err(e) => {
                    error!("Failed to remove existing installer: {}", e);
                    return Err(Box::new(InstallerError::DownloadFailed(
                        "Could not remove existing installer file. Please close any running installers and try again.".to_string()
                    )));
                }
            }
        }

        // Create the file with proper write permissions
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(installer_path) {
                Ok(file) => file,
                Err(e) => {
                    error!("Failed to create installer file: {}", e);
                    return Err(Box::new(InstallerError::DownloadFailed(
                        format!("Could not create installer file: {}", e)
                    )));
                }
            };

        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ReqwestErrorWrapper)?;
            match file.write_all(&chunk) {
                Ok(_) => {
                    downloaded += chunk.len() as u64;
                    if total_size > 0 {
                        let progress = downloaded as f32 / total_size as f32;
                        Self::send_message(&sender, InstallerMessage::UpdateProgress(progress))?;
                    }
                },
                Err(e) => {
                    error!("Failed to write installer chunk: {}", e);
                    // Try to clean up the partial file
                    drop(file);  // Ensure file is closed
                    let _ = std::fs::remove_file(installer_path);
                    return Err(Box::new(InstallerError::DownloadFailed(
                        format!("Failed to write installer: {}", e)
                    )));
                }
            }
        }

        // Explicitly close the file before launching
        drop(file);

        Self::send_message(&sender, InstallerMessage::SetState(InstallerState::Installing))?;

        // Launch the installer and wait for it to complete
        match TokioCommand::new(installer_path).spawn() {
            Ok(mut child) => {
                info!("Successfully launched Ninite installer");
                
                // Wait for the process to complete
                match child.wait().await {
                    Ok(_) => {
                        info!("Ninite installer completed successfully");
                        // Clean up the installer file after it's done
                        match std::fs::remove_file(installer_path) {
                            Ok(_) => info!("Cleaned up installer file"),
                            Err(e) => warn!("Could not clean up installer file: {}", e),
                        }
                    },
                    Err(e) => {
                        error!("Failed to wait for installer: {}", e);
                        // Try to clean up the file even if we couldn't wait for the process
                        let _ = std::fs::remove_file(installer_path);
                        return Err(Box::new(InstallerError::DownloadFailed(
                            format!("Failed to wait for installer: {}", e)
                        )));
                    }
                }
            },
            Err(e) => {
                error!("Failed to launch installer: {}", e);
                // Clean up the installer file
                let _ = std::fs::remove_file(installer_path);
                return Err(Box::new(InstallerError::DownloadFailed(
                    format!("Failed to launch installer: {}", e)
                )));
            }
        }

        Self::send_message(&sender, InstallerMessage::SetState(InstallerState::Idle))?;
        Ok(())
    }

    fn send_message(sender: &Sender<InstallerMessage>, message: InstallerMessage) -> InstallerResult<()> {
        sender.send(message).map_err(SendErrorWrapper)?;
        Ok(())
    }
}

impl eframe::App for DevDashboard {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let delta_time = now.duration_since(self.last_frame_time).as_secs_f32();
        self.last_frame_time = now;

        // Only check Ninite and app status every 2 seconds
        if now.duration_since(self.last_check) >= Duration::from_secs(2) {
            let mut sys = System::new_all();
            sys.refresh_processes();
            
            self.ninite_running = sys.processes().iter().any(|(_, process)| {
                let name = process.name().to_lowercase();
                name.contains("ninite") && name.ends_with(".exe")
            });

            // Only update app states if Ninite is not running
            if !self.ninite_running {
                for app in &mut self.ninite_apps {
                    app.check_installation();
                }
            }

            self.last_check = now;
        }

        // Prevent tab switching during installation
        if self.installer_state != InstallerState::Idle {
            self.current_tab = Tab::Tools;
        }

        self.current_cpu_usage.update(delta_time);
        self.memory_usage.update(delta_time);
        for usage in self.disk_usage.values_mut() {
            usage.update(delta_time);
        }
        if let Some(gpu_info) = &mut self.gpu_info {
            gpu_info.memory_usage.update(delta_time);
            gpu_info.gpu_usage.update(delta_time);
        }

        if self.last_network_update.elapsed() >= Duration::from_millis(100) {
            self.sys.refresh_networks();
            let networks = self.sys.networks();
            
            for (name, stats) in self.network_stats.iter_mut() {
                if let Some((_, data)) = networks
                    .iter()
                    .find(|(n, _)| *n == name) {
                    let current_received = data.received();
                    let current_sent = data.transmitted();
                    let elapsed = stats.last_update.elapsed().as_secs_f64();
                    
                    if elapsed > 0.0 {
                        let received_diff = if current_received < stats.last_received {
                            current_received
                        } else {
                            current_received - stats.last_received
                        };
                        
                        let sent_diff = if current_sent < stats.last_sent {
                            current_sent
                        } else {
                            current_sent - stats.last_sent
                        };
                        
                        stats.received_speed = received_diff as f64 / elapsed;
                        stats.sent_speed = sent_diff as f64 / elapsed;
                        
                        stats.total_received = stats.total_received.saturating_add(received_diff);
                        stats.total_sent = stats.total_sent.saturating_add(sent_diff);
                    }
                    
                    stats.last_received = current_received;
                    stats.last_sent = current_sent;
                    stats.last_update = Instant::now();
                }
            }
            
            self.last_network_update = Instant::now();
        }

        if self.last_update.elapsed() >= Duration::from_secs(1) {
            self.sys.refresh_cpu();
            self.sys.refresh_memory();
            self.sys.refresh_disks();
            
            let total_usage: f32 = match self.sys.cpus().len() {
                0 => {
                    0.0
                },
                len => {
                    let usage: f32 = self.sys.cpus().iter().map(|cpu| cpu.cpu_usage()).sum::<f32>();
                    (usage / len as f32).min(100.0)
                }
            };
            self.current_cpu_usage.set_target(total_usage);

            let total_memory = self.sys.total_memory() as f64;
            if total_memory > 0.0 {
                let used_memory = (total_memory - self.sys.available_memory() as f64) / total_memory;
                self.memory_usage.set_target((used_memory as f32).min(1.0));
            }

            for disk in self.sys.disks() {
                let mount_point = disk.mount_point().to_string_lossy().to_string();
                if mount_point.len() == 2 && mount_point.ends_with(":") {
                    let total_space = disk.total_space();
                    if total_space > 0 {
                        let usage = (total_space - disk.available_space()) as f64 / total_space as f64;
                        self.disk_usage
                            .entry(mount_point.clone())
                            .or_insert_with(|| AnimatedValue::new(usage as f32))
                            .set_target((usage as f32).min(1.0));
                    }
                }
            }

            let networks = self.sys.networks();
            let current_networks: Vec<String> = networks
                .iter()
                .filter(|(name, _)| DevDashboard::is_physical_interface(name))
                .map(|(name, _)| name.to_string())
                .collect();
            
            self.network_stats.retain(|name, _| current_networks.contains(name));
            
            for name in &current_networks {
                if !self.network_stats.contains_key(name as &str) {
                    if let Some((_, data)) = networks.iter().find(|(n, _)| *n == name) {
                        self.network_stats.insert(name.clone(), NetworkStats {
                            total_received: data.received(),
                            total_sent: data.transmitted(),
                            last_received: data.received(),
                            last_sent: data.transmitted(),
                            received_speed: 0.0,
                            sent_speed: 0.0,
                            last_update: Instant::now(),
                        });
                    }
                }
            }

            self.update_gpu_info();

            self.last_update = Instant::now();
        }

        ctx.request_repaint_after(Duration::from_secs_f32(1.0 / 60.0));

        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(17, 24, 39);
        visuals.window_fill = egui::Color32::from_rgb(17, 24, 39);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(31, 41, 55);
        visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(243, 244, 246);
        visuals.window_rounding = 12.0.into();
        visuals.window_shadow.extrusion = 2.0;
        ctx.set_visuals(visuals);

        // Add top panel with welcome message
        egui::TopBottomPanel::top("top_panel")
            .frame(egui::Frame::none()
                .fill(egui::Color32::from_rgb(31, 41, 55))
                .inner_margin(egui::style::Margin::symmetric(10.0, 8.0)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let system_username = username();
                    let display_name = self.settings.custom_username.as_deref()
                        .unwrap_or_else(|| system_username.as_str());
                    let capitalized_name = display_name.chars().next()
                        .map(|c| c.to_uppercase().collect::<String>())
                        .unwrap_or_default() + &display_name[1..];
                    ui.heading(format!("Welcome back, {}!", capitalized_name));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("⚙").on_hover_text("Settings").clicked() {
                            self.show_settings = true;
                        }
                        ui.label(format!("v0.1.1-beta.3"));
                    });
                });
            });

        // Show settings window if enabled
        self.show_settings_window(ctx);

        // Add tabs panel
        if !self.ninite_running {
            egui::TopBottomPanel::top("tabs")
                .frame(egui::Frame::none()
                    .fill(egui::Color32::from_rgb(31, 41, 55))
                    .inner_margin(egui::style::Margin::symmetric(8.0, 4.0)))
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut self.current_tab, Tab::Dashboard, "Dashboard");
                        ui.selectable_value(&mut self.current_tab, Tab::Tools, "Tools");
                    });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let available_width = ui.available_width();
            
            let min_card_width = 280.0;
            let spacing = 16.0;
            
            let max_columns = 3;
            let columns = if available_width >= (min_card_width + spacing) * 3.2 {
                max_columns
            } else if available_width >= (min_card_width + spacing) * 2.2 {
                2
            } else {
                1
            };
            
            // Show content based on selected tab
            if self.installer_state == InstallerState::Idle {
                match self.current_tab {
                    Tab::Dashboard => {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let base_frame = egui::Frame::none()
                                .fill(egui::Color32::from_rgb(31, 41, 55))
                                .inner_margin(egui::style::Margin::same(16.0))
                                .rounding(12.0)
                                .shadow(egui::epaint::Shadow {
                                    extrusion: 2.0,
                                    color: egui::Color32::from_black_alpha(60),
                                });

                            ui.columns(columns, |columns| {
                                for i in 0..columns.len() {
                                    columns[i].add_space(spacing);
                                    match i {
                                        0 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_system_card(ui);
                                        }),
                                        1 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_cpu_card(ui);
                                        }),
                                        2 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_memory_card(ui);
                                        }),
                                        _ => unreachable!(),
                                    };
                                }

                                for column in columns.iter_mut() {
                                    column.add_space(spacing);
                                }

                                for i in 0..columns.len() {
                                    match i {
                                        0 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_storage_card(ui);
                                        }),
                                        1 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_network_card(ui);
                                        }),
                                        2 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_gpu_card(ui);
                                        }),
                                        _ => unreachable!(),
                                    };
                                }

                                if columns.len() == 1 {
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_cpu_card(ui);
                                    });
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_memory_card(ui);
                                    });
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_network_card(ui);
                                    });
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_gpu_card(ui);
                                    });
                                } else if columns.len() == 2 {
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_memory_card(ui);
                                    });
                                    
                                    columns[1].add_space(spacing);
                                    base_frame.show(&mut columns[1], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_gpu_card(ui);
                                    });
                                }
                            });
                        });
                    },
                    Tab::Tools => {
                        self.show_tools_tab(ui);
                    },
                }
            }
        });

        // Show overlay message when installer is running
        if self.ninite_running {
            let screen_rect = ctx.screen_rect();
            let window_size = egui::Vec2::new(400.0, 150.0);
            let pos = screen_rect.center() - (window_size * 0.5);
            
            egui::Window::new("installer_overlay")
                .fixed_pos(pos)
                .fixed_size(window_size)
                .title_bar(false)
                .frame(egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(240))
                    .rounding(8.0)
                    .inner_margin(egui::style::Margin::same(20.0))
                    .shadow(egui::epaint::Shadow {
                        extrusion: 8.0,
                        color: egui::Color32::from_black_alpha(100),
                    }))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("Installation in Progress");
                        ui.add_space(8.0);
                        ui.label("The Ninite installer is running.");
                        ui.label("Please complete the installation before making any changes.");
                        ui.add_space(8.0);
                        ui.label("This window will be enabled once the installation is complete.");
                    });
                });
        }

        // Prevent tab switching during installation
        if self.ninite_running {
            self.current_tab = Tab::Tools;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let available_width = ui.available_width();
            
            let min_card_width = 280.0;
            let spacing = 16.0;
            
            let max_columns = 3;
            let columns = if available_width >= (min_card_width + spacing) * 3.2 {
                max_columns
            } else if available_width >= (min_card_width + spacing) * 2.2 {
                2
            } else {
                1
            };
            
            // Show content based on selected tab
            if !self.ninite_running {
                match self.current_tab {
                    Tab::Dashboard => {
                        let scroll_area = egui::ScrollArea::vertical().id_source("dashboard_scroll");
                        scroll_area.show(ui, |ui| {
                            let base_frame = egui::Frame::none()
                                .fill(egui::Color32::from_rgb(31, 41, 55))
                                .inner_margin(egui::style::Margin::same(16.0))
                                .rounding(12.0)
                                .shadow(egui::epaint::Shadow {
                                    extrusion: 2.0,
                                    color: egui::Color32::from_black_alpha(60),
                                });

                            ui.columns(columns, |columns| {
                                for i in 0..columns.len() {
                                    columns[i].add_space(spacing);
                                    match i {
                                        0 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_system_card(ui);
                                        }),
                                        1 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_cpu_card(ui);
                                        }),
                                        2 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_memory_card(ui);
                                        }),
                                        _ => unreachable!(),
                                    };
                                }

                                for column in columns.iter_mut() {
                                    column.add_space(spacing);
                                }

                                for i in 0..columns.len() {
                                    match i {
                                        0 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_storage_card(ui);
                                        }),
                                        1 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_network_card(ui);
                                        }),
                                        2 => base_frame.show(&mut columns[i], |ui| {
                                            ui.set_min_width(min_card_width);
                                            ui.set_min_height(180.0);
                                            self.show_gpu_card(ui);
                                        }),
                                        _ => unreachable!(),
                                    };
                                }

                                if columns.len() == 1 {
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_cpu_card(ui);
                                    });
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_memory_card(ui);
                                    });
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_network_card(ui);
                                    });
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_gpu_card(ui);
                                    });
                                } else if columns.len() == 2 {
                                    columns[0].add_space(spacing);
                                    base_frame.show(&mut columns[0], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_memory_card(ui);
                                    });
                                    
                                    columns[1].add_space(spacing);
                                    base_frame.show(&mut columns[1], |ui| {
                                        ui.set_min_width(min_card_width);
                                        ui.set_min_height(180.0);
                                        self.show_gpu_card(ui);
                                    });
                                }
                            });
                        });
                    },
                    Tab::Tools => {
                        let scroll_area = egui::ScrollArea::vertical().id_source("tools_scroll");
                        scroll_area.show(ui, |ui| {
                            self.show_tools_tab(ui);
                        });
                    },
                }
            }
        });

        // Show overlay message when installer is running
        if self.ninite_running {
            let screen_rect = ctx.screen_rect();
            let window_size = egui::Vec2::new(400.0, 150.0);
            let pos = screen_rect.center() - (window_size * 0.5);
            
            egui::Window::new("installer_overlay")
                .fixed_pos(pos)
                .fixed_size(window_size)
                .title_bar(false)
                .frame(egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(240))
                    .rounding(8.0)
                    .inner_margin(egui::style::Margin::same(20.0))
                    .shadow(egui::epaint::Shadow {
                        extrusion: 8.0,
                        color: egui::Color32::from_black_alpha(100),
                    }))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("Installation in Progress");
                        ui.add_space(8.0);
                        ui.label("The Ninite installer is running.");
                        ui.label("Please complete the installation before making any changes.");
                        ui.add_space(8.0);
                        ui.label("This window will be enabled once the installation is complete.");
                    });
                });
        }

        // Prevent tab switching during installation
        if self.ninite_running {
            self.current_tab = Tab::Tools;
        }
    }
}

impl DevDashboard {
    /// Helper function to display a card in the UI with consistent styling
    /// title: Card title
    /// add_contents: Function to add card contents
    fn show_card(&self, ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
        ui.label(RichText::new(title)
            .strong()
            .heading());
        ui.add_space(8.0);
        add_contents(ui);
    }

    /// Displays system information card
    /// Shows OS details, hostname, and uptime
    fn show_system_card(&self, ui: &mut egui::Ui) {
        self.show_card(ui, "System", |ui| {
            let uptime_secs = self.sys.uptime();
            let uptime_hours = uptime_secs / 3600;
            let uptime_minutes = (uptime_secs % 3600) / 60;

            ui.label(format!("OS: {} {}", 
                self.sys.name().unwrap_or_default(),
                self.sys.os_version().unwrap_or_default()
            ));
            ui.label(format!("Hostname: {}", self.sys.host_name().unwrap_or_default()));
            ui.label(format!("Uptime: {} hours, {} minutes", 
                uptime_hours,
                uptime_minutes
            ));
        });
    }

    /// Displays CPU information card
    /// Shows CPU model, cores, threads, speed, and usage
    fn show_cpu_card(&self, ui: &mut egui::Ui) {
        self.show_card(ui, "CPU", |ui| {
            if let Some(cpu) = self.sys.cpus().first() {
                ui.label(format!("Model: {}", cpu.brand()));
                ui.label(format!("Physical Cores: {}", self.sys.physical_core_count().unwrap_or(0)));
                ui.label(format!("Threads: {}", self.sys.cpus().len()));
                ui.label(format!("Speed: {:.1} GHz", cpu.frequency() as f64 / 1000.0));
                
                ui.add_space(4.0);
                ui.label("Usage:");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(format!("Usage ({:.1}%)", self.current_cpu_usage.current));
                });
                let visuals = ui.visuals_mut();
                visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(55, 65, 81);
                ui.add(egui::ProgressBar::new(self.current_cpu_usage.current / 100.0)
                    .fill(egui::Color32::from_rgb(37, 99, 235)));
            }
        });
    }

    /// Displays memory information card
    /// Shows total memory, used memory, free memory, and usage percentage
    fn show_memory_card(&self, ui: &mut egui::Ui) {
        self.show_card(ui, "Memory", |ui| {
            let total_gb = self.sys.total_memory() as f64 / (1024.0 * 1024.0 * 1024.0);
            let used_gb = (self.sys.total_memory() as f64 * self.memory_usage.current as f64) / (1024.0 * 1024.0 * 1024.0);
            let free_gb = total_gb - used_gb;
            let usage_percentage = self.memory_usage.current * 100.0;

            ui.label(format!("Total: {:.1} GB", total_gb));
            
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(format!("Used: {:.1} GB ({:.1}%)", used_gb, usage_percentage));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(format!("Free: {:.1} GB", free_gb));
                });
            });
            let visuals = ui.visuals_mut();
            visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(55, 65, 81);
            ui.add(egui::ProgressBar::new(self.memory_usage.current)
                .fill(egui::Color32::from_rgb(22, 163, 74)));
        });
    }

    /// Displays storage information card
    /// Shows disk usage for each drive with detailed statistics
    fn show_storage_card(&mut self, ui: &mut egui::Ui) {
        self.show_card(ui, "Storage", |ui| {
            for disk in self.sys.disks() {
                let mount_point = disk.mount_point().to_string_lossy();
                
                // Use Windows API to get accurate disk space information
                if let Some((total_bytes, free_bytes)) = Self::get_disk_space(&mount_point) {
                    if total_bytes == 0 {
                        warn!("Drive {} reported zero total space", mount_point);
                        ui.label(RichText::new(format!("{} (Unable to read size)", mount_point)).strong());
                        ui.label("Could not read disk information. Try running as administrator.");
                        ui.add_space(12.0);
                        continue;
                    }
                    
                    let available_bytes = free_bytes;
                    let used_bytes = total_bytes - available_bytes;
                    
                    let (total, total_unit) = DevDashboard::format_bytes(total_bytes);
                    let (_used, _) = DevDashboard::format_bytes(used_bytes);
                    let free_gb = available_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    
                    let usage_percentage = if let Some(usage) = self.disk_usage.get(&mount_point.to_string()) {
                        usage.current * 100.0
                    } else {
                        (used_bytes as f32 / total_bytes as f32) * 100.0
                    };
                    
                    ui.label(RichText::new(format!("{} ({:.1} {})", mount_point, total, total_unit)).strong());
                    ui.horizontal(|ui| {
                        ui.label(format!("Used ({:.1}%)", usage_percentage));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(format!("Free: {:.1} GB", free_gb));
                        });
                    });
                    
                    let progress = if let Some(usage) = self.disk_usage.get(&mount_point.to_string()) {
                        usage.current
                    } else {
                        used_bytes as f32 / total_bytes as f32
                    };
                    
                    let visuals = ui.visuals_mut();
                    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(55, 65, 81);
                    let progress_bar = egui::ProgressBar::new(progress)
                        .fill(egui::Color32::from_rgb(202, 138, 4));
                    ui.add(progress_bar);
                    
                    let free_space = available_bytes as f64;
                    let free_space_str = if free_space >= 1024.0 * 1024.0 * 1024.0 * 1024.0 {
                        format!("Free: {:.1} TB", free_space / (1024.0 * 1024.0 * 1024.0 * 1024.0))
                    } else {
                        format!("Free: {:.1} GB", free_space / (1024.0 * 1024.0 * 1024.0))
                    };
                    ui.label(free_space_str);
                } else {
                    error!("Failed to get disk space for {}", mount_point);
                    ui.label(RichText::new(format!("{} (Error)", mount_point)).strong());
                    ui.label("Failed to read disk information. Try running as administrator.");
                }
                
                ui.add_space(12.0);
            }
        });
    }

    /// Formats byte values into human-readable sizes
    /// Returns a tuple of (value, unit) where unit is B, KB, MB, GB, or TB
    fn format_bytes(bytes: u64) -> (f64, &'static str) {
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;
        const TB: f64 = GB * 1024.0;

        let bytes = bytes as f64;
        
        if bytes >= TB {
            (bytes / TB, "TB")
        } else if bytes >= GB {
            (bytes / GB, "GB")
        } else if bytes >= MB {
            (bytes / MB, "MB")
        } else if bytes >= KB {
            (bytes / KB, "KB")
        } else {
            (bytes, "B")
        }
    }

    /// Displays network information card
    /// Shows network interface statistics including speed and total usage
    fn show_network_card(&self, ui: &mut egui::Ui) {
        self.show_card(ui, "Network", |ui| {
            for (name, _data) in self.sys.networks() {
                let name_lower = name.to_string().to_lowercase();
                
                // Filter for physical network interfaces
                let is_physical = name_lower.contains("ethernet") ||
                                name_lower.starts_with("eth") ||
                                name_lower.starts_with("en") ||
                                name_lower.starts_with("wlan") ||
                                name_lower.starts_with("wi-fi") ||
                                name_lower.starts_with("wireless") ||
                                name_lower.contains("wireless");
                
                if !is_physical {
                    continue;
                }
                
                // Display interface type
                if name_lower.contains("wireless") || 
                   name_lower.contains("wi-fi") || 
                   name_lower.starts_with("wlan") {
                    ui.label("Wi-Fi");
                } else {
                    ui.label("Ethernet");
                }
                
                ui.add_space(4.0);
                
                // Display network statistics if available
                if let Some(stats) = self.network_stats.get(name) {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label("Received");
                            let total_received = stats.total_received as f64;
                            let total_text = if total_received >= 1024.0 * 1024.0 * 1024.0 {
                                format!("{:.1} GB", total_received / (1024.0 * 1024.0 * 1024.0))
                            } else if total_received >= 1024.0 * 1024.0 {
                                format!("{:.1} MB", total_received / (1024.0 * 1024.0))
                            } else {
                                format!("{:.1} KB", total_received / 1024.0)
                            };
                            ui.label(RichText::new(total_text)
                                .size(16.0)
                                .strong());
                            ui.label(RichText::new(format!("{:.1} kb/s", stats.received_speed / 1024.0))
                                .color(egui::Color32::from_rgb(88, 165, 237)));
                        });
                        ui.add_space(32.0); // Add fixed margin between received and sent data
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.vertical(|ui| {
                                ui.label("Sent");
                                let total_sent = stats.total_sent as f64;
                                let total_text = if total_sent >= 1024.0 * 1024.0 * 1024.0 {
                                    format!("{:.1} GB", total_sent / (1024.0 * 1024.0 * 1024.0))
                                } else if total_sent >= 1024.0 * 1024.0 {
                                    format!("{:.1} MB", total_sent / (1024.0 * 1024.0))
                                } else {
                                    format!("{:.1} KB", total_sent / 1024.0)
                                };
                                ui.label(RichText::new(total_text)
                                    .size(16.0)
                                    .strong());
                                ui.label(RichText::new(format!("{:.1} kb/s", stats.sent_speed / 1024.0))
                                    .color(egui::Color32::from_rgb(67, 208, 118)));
                            });
                        });
                    });
                }
            }
        });
    }

    /// Displays GPU information card
    /// Shows GPU model, driver version, usage, temperature, and memory usage
    fn show_gpu_card(&mut self, ui: &mut egui::Ui) {
        if let Some(gpu_info) = &self.gpu_info {
            self.show_card(ui, "GPU", |ui| {
                ui.label(RichText::new(&gpu_info.name).strong());
                if let Some(driver) = &gpu_info.driver_version {
                    ui.label(format!("Driver: {}", driver));
                }
                if let Some(pci_id) = &gpu_info.pci_bus_id {
                    ui.label(format!("Bus ID: {}", pci_id));
                }
                ui.add_space(8.0);

                ui.label("GPU Usage:");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(format!("Usage ({:.1}%)", gpu_info.gpu_usage.current * 100.0));
                });
                let visuals = ui.visuals_mut();
                visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(55, 65, 81);
                ui.add(egui::ProgressBar::new(gpu_info.gpu_usage.current)
                    .fill(egui::Color32::from_rgb(220, 38, 38)));

                if let Some(temp) = gpu_info.temperature {
                    ui.label(format!("Temperature: {}°C", temp));
                }

                if let (Some(total), Some(used)) = (gpu_info.memory_total, gpu_info.memory_used) {
                    ui.add_space(8.0);
                    let total_gb = total as f64 / 1024.0 / 1024.0 / 1024.0;
                    let used_gb = used as f64 / 1024.0 / 1024.0 / 1024.0;
                    ui.horizontal(|ui| {
                        ui.label(format!("Memory ({:.1}%)", gpu_info.memory_usage.current * 100.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(format!("{:.1} GB / {:.1} GB", used_gb, total_gb));
                        });
                    });
                    let visuals = ui.visuals_mut();
                    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(55, 65, 81);
                    ui.add(egui::ProgressBar::new(gpu_info.memory_usage.current)
                        .fill(egui::Color32::from_rgb(147, 51, 234)));
                }
            });
        } else {
            self.show_card(ui, "GPU", |ui| {
                ui.label("No GPU detected");
            });
        }
    }

    /// Checks if a network interface name represents a physical interface
    /// Filters out virtual interfaces and loopback
    fn is_physical_interface(name: &str) -> bool {
        let name_lower = name.to_lowercase();
        name_lower.contains("ethernet") ||
        name_lower.starts_with("eth") ||
        name_lower.starts_with("en") ||
        name_lower.starts_with("wlan") ||
        name_lower.starts_with("wi-fi") ||
        name_lower.starts_with("wireless") ||
        name_lower.contains("wireless")
    }

    /// Initializes GPU information using either NVML (for NVIDIA GPUs) or WMI (for other GPUs)
    /// Returns None if no suitable GPU is found
    fn initialize_gpu() -> Option<GpuInfo> {
        // Try NVIDIA GPU first using NVML
        match Nvml::init() {
            Ok(nvml) => {
                match nvml.device_by_index(0) {
                    Ok(device) => {
                        let name = device.name().unwrap_or_else(|_| "Unknown GPU".to_string());
                        let mut gpu_info = GpuInfo::new(name);
                        
                        if let Ok(pci_info) = device.pci_info() {
                            gpu_info.pci_bus_id = Some(format!("{:04x}:{:02x}:{:02x}.0", 
                                pci_info.domain, 
                                pci_info.bus, 
                                pci_info.device
                            ));
                        }
                        
                        if let Ok(version) = nvml.sys_driver_version() {
                            gpu_info.driver_version = Some(version);
                        }
                        
                        info!("Successfully initialized NVIDIA GPU: {} (Driver: {})", 
                            gpu_info.name,
                            gpu_info.driver_version.as_deref().unwrap_or("Unknown")
                        );
                        
                        return Some(gpu_info);
                    }
                    Err(e) => {
                        warn!("Failed to get NVIDIA device: {}", e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to initialize NVML: {}", e);
            }
        }

        // Fallback to WMI for non-NVIDIA GPUs
        if let Ok(com_con) = COMLibrary::new() {
            if let Ok(wmi_con) = WMIConnection::new(com_con) {
                #[derive(serde::Deserialize, Debug)]
                #[serde(rename = "Win32_VideoController")]
                struct Win32VideoController {
                    #[serde(rename = "Name")]
                    name: String,
                    #[serde(rename = "AdapterRAM")]
                    adapter_ram: Option<u64>,
                    #[serde(rename = "DriverVersion")]
                    driver_version: Option<String>,
                    #[serde(rename = "PNPDeviceID")]
                    device_id: Option<String>,
                }

                if let Ok(results) = wmi_con.query::<Win32VideoController>() {
                    for gpu in results {
                        if !gpu.name.to_lowercase().contains("microsoft basic display") {
                            let mut gpu_info = GpuInfo::new(gpu.name);
                            gpu_info.memory_total = gpu.adapter_ram;
                            gpu_info.driver_version = gpu.driver_version;
                            
                            if let Some(device_id) = gpu.device_id {
                                if device_id.starts_with("PCI\\") {
                                    if let Some(ven_start) = device_id.find("VEN_") {
                                        if let Some(dev_start) = device_id.find("DEV_") {
                                            let vendor = &device_id[ven_start + 4..ven_start + 8];
                                            let device = &device_id[dev_start + 4..dev_start + 8];
                                            gpu_info.pci_bus_id = Some(format!("0000:00:00.0 [{}:{}]", vendor, device));
                                        }
                                    }
                                }
                            }
                            
                            info!("Found GPU through WMI: {} (Driver: {})", 
                                gpu_info.name,
                                gpu_info.driver_version.as_deref().unwrap_or("Unknown")
                            );
                            
                            return Some(gpu_info);
                        }
                    }
                }
            }
        }

        warn!("No suitable GPU found");
        None
    }

    /// Updates GPU information including usage, temperature, and memory usage
    /// Uses either NVML or WMI depending on GPU type
    fn update_gpu_info(&mut self) {
        if let Some(gpu_info) = &mut self.gpu_info {
            if let Some(nvml) = &self.nvml {
                if let Ok(device) = nvml.device_by_index(0) {
                    if let Ok(memory) = device.memory_info() {
                        gpu_info.memory_total = Some(memory.total);
                        gpu_info.memory_used = Some(memory.used);
                        gpu_info.memory_usage.set_target((memory.used as f32 / memory.total as f32).min(1.0));
                    }

                    if let Ok(utilization) = device.utilization_rates() {
                        gpu_info.utilization = Some(utilization.gpu as f32);
                        gpu_info.gpu_usage.set_target((utilization.gpu as f32 / 100.0).min(1.0));
                    }
                }
            } else {
                // Fallback to WMI for non-NVIDIA GPUs
                if let Ok(com_con) = COMLibrary::new() {
                    if let Ok(wmi_con) = WMIConnection::new(com_con) {
                        #[derive(serde::Deserialize)]
                        #[serde(rename = "Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine")]
                        struct GpuPerformance {
                            #[serde(rename = "UtilizationPercentage")]
                            utilization: Option<u32>,
                        }

                        if let Ok(results) = wmi_con.query::<GpuPerformance>() {
                            if let Some(perf) = results.into_iter().next() {
                                if let Some(util) = perf.utilization {
                                    gpu_info.utilization = Some(util as f32);
                                    gpu_info.gpu_usage.set_target((util as f32 / 100.0).min(1.0));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Main entry point of the application
/// Sets up logging and initializes the GUI
fn main() -> Result<(), eframe::Error> {
    // Initialize logging to file
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open("dev_dashboard.log")
        .expect("Failed to create log file");

    WriteLogger::init(
        LevelFilter::Debug,
        Config::default(),
        log_file,
    ).expect("Failed to initialize logger");

    info!("Starting Dev Dashboard");

    // Configure window options
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_transparent(true),
        ..Default::default()
    };

    info!("Initializing application window");
    let result = eframe::run_native(
        "Dev Dashboard",
        options,
        Box::new(|_cc| Box::new(DevDashboard::default())),
    );

    if let Err(ref e) = result {
        error!("Application crashed: {}", e);
    }

    result
} 